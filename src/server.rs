//! The MCP server surface: 30 tools, 3 resources, and 7 prompts, matching the
//! Python server's contract. Tools never raise to the client — failures come back
//! as `{"error": ..., "message": ...}` dicts (see `crate::error`).

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, GetPromptResult, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, PromptMessage, ReadResourceRequestParams, ReadResourceResult, Resource,
    ResourceContents, ResourceTemplate, Role, ServerCapabilities, ServerInfo,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::{prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::analytics::{
    aggregate_period, compare_periods_by_dimension, contains_filter, drop_incomplete_rows,
    execute_search_analytics, expected_ctr, fetch_all_rows, fetch_all_rows_concurrent,
    filename_fragment, getf, num, re2_escape, resolve_comparison_periods, resolve_dates, round_to,
    rows_to_csv, single_filter, sort_maps_by, sort_maps_by_str, top_by_dimension, validate_date,
    validate_site_url, AppState, DimensionFilterGroup, JsonMap, SaQuery, CTR_PRECISION,
    DEFAULT_WINDOW_DAYS, FRESH_DATA_LAG_DAYS, MAX_PAGINATED_ROWS, MAX_ROW_LIMIT,
    POSITION_BUCKETS, POSITION_PRECISION, QUICK_WIN_TARGET_CTR,
};
use crate::error::GscError;
use crate::prompts;

#[derive(Clone)]
pub struct GscServer {
    state: Arc<AppState>,
}

impl GscServer {
    pub fn new(state: Arc<AppState>) -> Self {
        GscServer { state }
    }
}

/// Directory CSV exports are written to: `$GSC_EXPORT_DIR`, else `<home>/Downloads`.
/// A model-supplied `output_path` is confined inside this root — an MCP tool argument
/// is untrusted input and must not become an arbitrary-file-write primitive.
fn export_root() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("GSC_EXPORT_DIR") {
        return std::path::PathBuf::from(dir);
    }
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("Downloads")
}

/// Server-side ceiling on any model-supplied `max_rows` (5× the paginated default,
/// = 1,000,000). Tool arguments come from a model that prompt-injected data can steer,
/// so an absurd value would mean unbounded API calls and memory. Clamped, not rejected —
/// the caller still gets its rows, just not an unbounded number of them.
const HARD_MAX_ROWS: u64 = MAX_PAGINATED_ROWS * 5;

/// Per-call ceiling on `batch_inspect` URLs. The URL Inspection API is ~2000
/// requests/day, so an unbounded list can burn the daily quota in one tool call.
const MAX_BATCH_INSPECT_URLS: usize = 50;

/// Windows reserved device names. `CON`, `NUL.csv` and friends are devices, not files,
/// whatever the extension. Rejected on every platform so a path check that looks fine
/// on Linux doesn't wave through something the same repo mishandles on Windows.
const WINDOWS_RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Lexical half of the export-path confinement: `output_path` must be a plain relative
/// `.csv` path with no traversal, no colons and no device names. The filesystem half
/// (symlinks/junctions) is re-checked at write time in `write_export_confined`.
fn validate_export_rel_path(custom: &str) -> Result<&std::path::Path, GscError> {
    use std::path::Component;
    let rel = std::path::Path::new(custom);
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(GscError::invalid(
            "output_path must be a plain relative path — absolute paths and \
             '..' are rejected. It is resolved inside the export directory \
             (~/Downloads, or set GSC_EXPORT_DIR to change it).",
        ));
    }
    for c in rel.components() {
        let Component::Normal(raw) = c else { continue };
        let name = raw.to_string_lossy();
        // A colon opens a Windows alternate data stream: "existing.txt:x.csv" writes a
        // hidden stream on someone else's file and still ends in .csv. The export ROOT
        // may legitimately contain "C:", but a relative component never needs one.
        if name.contains(':') {
            return Err(GscError::invalid(
                "output_path must not contain ':' — alternate data streams are rejected.",
            ));
        }
        let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
        if WINDOWS_RESERVED_NAMES.contains(&stem.as_str()) {
            return Err(GscError::invalid(format!(
                "output_path component '{name}' is a reserved device name."
            )));
        }
    }
    if !custom.to_ascii_lowercase().ends_with(".csv") {
        return Err(GscError::invalid("output_path must end in .csv"));
    }
    Ok(rel)
}

/// Write the CSV export, verifying against the real filesystem — immediately before the
/// open — that the target still lives inside the export root. The lexical checks can't
/// see symlinks or junctions, and a link planted under the root would otherwise turn the
/// export into an arbitrary-file-write primitive.
fn write_export_confined(
    root: &std::path::Path,
    target: &std::path::Path,
    data: &[u8],
) -> Result<(), GscError> {
    let io = |e: std::io::Error| GscError::Api {
        status: None,
        raw: format!("failed writing export file {}: {e}", target.display()),
    };
    let outside = || {
        GscError::invalid("output_path resolves outside the export directory (symlink or junction?)")
    };
    // The root must exist before it can be canonicalized.
    std::fs::create_dir_all(root).map_err(io)?;
    let real_root = root.canonicalize().map_err(io)?;
    // Check the deepest existing ancestor first, so a path that escapes can't even
    // create directories on the way out.
    if let Some(existing) = target.ancestors().skip(1).find(|a| a.exists()) {
        if !existing.canonicalize().map_err(io)?.starts_with(&real_root) {
            return Err(outside());
        }
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
        if !parent.canonicalize().map_err(io)?.starts_with(&real_root) {
            return Err(outside());
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    // create_new: refuse to clobber an existing file — exports only add.
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        // An export can hold a property's whole query corpus — keep it owner-only.
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(target).map_err(io)?;
    std::io::Write::write_all(&mut f, data).map_err(io)
}

/// Convert a tool outcome into a CallToolResult, mapping errors to the
/// structured error dict (Python `gsc_tool` contract).
fn to_result(r: Result<Value, GscError>) -> Result<CallToolResult, McpError> {
    let value = match r {
        // Success payloads are size-capped so a big pull can't flood the client's
        // context (mirrors the Python server's _truncate_for_chat in @gsc_tool).
        Ok(v) => crate::analytics::truncate_for_chat(v),
        Err(e) => {
            tracing::warn!("tool error: {}", e.message());
            e.to_error_value()
        }
    };
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

// --- serde defaults ---

fn d_web() -> String {
    "web".to_string()
}
fn d_auto() -> String {
    "auto".to_string()
}
fn d_clicks() -> String {
    "clicks".to_string()
}
fn d_delta_clicks() -> String {
    "delta_clicks".to_string()
}
fn d_en() -> String {
    "en".to_string()
}
fn d_row_limit() -> u64 {
    1000
}
fn d_max_paginated() -> u64 {
    MAX_PAGINATED_ROWS
}
fn d_limit_20() -> usize {
    20
}
fn d_limit_50() -> usize {
    50
}
fn d_limit_100() -> usize {
    100
}
fn d_days_14() -> i64 {
    14
}
fn d_days_28() -> i64 {
    28
}
fn d_min_impressions_10() -> u64 {
    10
}
fn d_min_impressions_50() -> u64 {
    50
}
fn d_min_impressions_100() -> u64 {
    100
}
fn d_min_clicks_5() -> u64 {
    5
}
fn d_min_previous_clicks_10() -> u64 {
    10
}
fn d_min_position() -> f64 {
    4.0
}
fn d_max_position() -> f64 {
    20.0
}

// --- Tool parameter structs ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchAnalyticsParams {
    /// The Search Console property URL (e.g. "https://example.com/" or "sc-domain:example.com")
    pub site_url: String,
    /// Start date in YYYY-MM-DD format (defaults to 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date in YYYY-MM-DD format (defaults to 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Dimensions to group by. Options: query, page, country, device, date, searchAppearance (default: ["query"])
    #[serde(default)]
    pub dimensions: Option<Vec<String>>,
    /// Filter groups, e.g. [{"filters": [{"dimension": "query", "operator": "contains", "expression": "python"}]}]
    #[serde(default)]
    pub dimension_filter_groups: Option<Vec<DimensionFilterGroup>>,
    /// Max rows to return (up to 25000)
    #[serde(default = "d_row_limit")]
    pub row_limit: u64,
    /// Pagination offset
    #[serde(default)]
    pub start_row: u64,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
    /// auto, byProperty, or byPage
    #[serde(default = "d_auto")]
    pub aggregation_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FreshDataParams {
    /// The Search Console property URL (e.g. "https://example.com/" or "sc-domain:example.com")
    pub site_url: String,
    /// Start date in YYYY-MM-DD format (defaults to 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date in YYYY-MM-DD format (defaults to yesterday)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Dimensions to group by. Options: query, page, country, device, date, searchAppearance (default: ["query"])
    #[serde(default)]
    pub dimensions: Option<Vec<String>>,
    /// Filter groups (same format as search_analytics)
    #[serde(default)]
    pub dimension_filter_groups: Option<Vec<DimensionFilterGroup>>,
    /// Max rows to return (up to 25000)
    #[serde(default = "d_row_limit")]
    pub row_limit: u64,
    /// Pagination offset
    #[serde(default)]
    pub start_row: u64,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
    /// auto, byProperty, or byPage
    #[serde(default = "d_auto")]
    pub aggregation_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComparePeriodsParams {
    /// The Search Console property URL
    pub site_url: String,
    /// First period start date (YYYY-MM-DD) — the "previous" period
    pub period1_start: String,
    /// First period end date (YYYY-MM-DD)
    pub period1_end: String,
    /// Second period start date (YYYY-MM-DD) — the "current" period
    pub period2_start: String,
    /// Second period end date (YYYY-MM-DD)
    pub period2_end: String,
    /// Dimensions to group by (default: ["query"])
    #[serde(default)]
    pub dimensions: Option<Vec<String>>,
    /// Optional filters (same format as search_analytics)
    #[serde(default)]
    pub dimension_filter_groups: Option<Vec<DimensionFilterGroup>>,
    /// Max rows fetched per period via pagination (default: 200000)
    #[serde(default = "d_max_paginated")]
    pub max_rows: u64,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectUrlParams {
    /// The fully-qualified URL to inspect (must be under the property)
    pub url: String,
    /// The Search Console property URL
    pub site_url: String,
    /// Language code for the results (default: "en")
    #[serde(default = "d_en")]
    pub language_code: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SiteUrlParams {
    /// The Search Console property URL
    pub site_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TopNParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Number of rows to return (default: 20)
    #[serde(default = "d_limit_20")]
    pub limit: usize,
    /// Sort by clicks, impressions, ctr, or position (default: clicks)
    #[serde(default = "d_clicks")]
    pub sort_by: String,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryPageReportParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Filter pages containing this string (optional)
    #[serde(default)]
    pub page_filter: Option<String>,
    /// Filter queries containing this string (optional)
    #[serde(default)]
    pub query_filter: Option<String>,
    /// Max rows (up to 25000)
    #[serde(default = "d_row_limit")]
    pub row_limit: u64,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuickWinsParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Minimum impressions to qualify (default: 100)
    #[serde(default = "d_min_impressions_100")]
    pub min_impressions: u64,
    /// Minimum average position (default: 4.0)
    #[serde(default = "d_min_position")]
    pub min_position: f64,
    /// Maximum average position (default: 20.0)
    #[serde(default = "d_max_position")]
    pub max_position: f64,
    /// Max rows to return (default: 100)
    #[serde(default = "d_limit_100")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrendingQueriesParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Number of days per period (default: 14)
    #[serde(default = "d_days_14")]
    pub days: i64,
    /// Minimum impressions in either period (default: 50)
    #[serde(default = "d_min_impressions_50")]
    pub min_impressions: u64,
    /// Sort field — delta_clicks, delta_impressions, delta_ctr, delta_position (default: delta_clicks)
    #[serde(default = "d_delta_clicks")]
    pub sort_by: String,
    /// Max rows to return (default: 50)
    #[serde(default = "d_limit_50")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContentDecayParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Number of days per period (default: 28)
    #[serde(default = "d_days_28")]
    pub days: i64,
    /// Minimum clicks in the previous period to qualify (default: 10)
    #[serde(default = "d_min_previous_clicks_10")]
    pub min_previous_clicks: u64,
    /// Max rows to return (default: 50)
    #[serde(default = "d_limit_50")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CannibalizationParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Minimum total impressions per query (default: 50)
    #[serde(default = "d_min_impressions_50")]
    pub min_impressions: u64,
    /// Max rows to return (default: 50)
    #[serde(default = "d_limit_50")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeviceBreakdownParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Optional — filter to pages containing this string
    #[serde(default)]
    pub page_filter: Option<String>,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CountryBreakdownParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Max countries to return (default: 20)
    #[serde(default = "d_limit_20")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SitemapParams {
    /// The Search Console property URL (e.g. "https://example.com/" or "sc-domain:example.com")
    pub site_url: String,
    /// The full URL of the sitemap (e.g. "https://example.com/sitemap.xml")
    pub sitemap_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchInspectParams {
    /// List of fully-qualified URLs to inspect (must be under the property)
    pub urls: Vec<String>,
    /// The Search Console property URL
    pub site_url: String,
    /// Language code for the results (default: "en")
    #[serde(default = "d_en")]
    pub language_code: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PerformanceSummaryParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Number of days for each period (default: 28)
    #[serde(default = "d_days_28")]
    pub days: i64,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DateTrendParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Optional filters (same format as search_analytics)
    #[serde(default)]
    pub dimension_filter_groups: Option<Vec<DimensionFilterGroup>>,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchAppearanceParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LandingPageQueriesParams {
    /// The Search Console property URL
    pub site_url: String,
    /// The exact page URL to analyze (e.g. "https://example.com/blog/post")
    pub page_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Max queries to return (default: 50)
    #[serde(default = "d_limit_50")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TopGrowingPagesParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Number of days per period (default: 14)
    #[serde(default = "d_days_14")]
    pub days: i64,
    /// Minimum clicks in either period to qualify (default: 5)
    #[serde(default = "d_min_clicks_5")]
    pub min_clicks: u64,
    /// Max pages to return (default: 50)
    #[serde(default = "d_limit_50")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NewLostQueriesParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Number of days per period (default: 28)
    #[serde(default = "d_days_28")]
    pub days: i64,
    /// Minimum impressions to qualify as significant (default: 10)
    #[serde(default = "d_min_impressions_10")]
    pub min_impressions: u64,
    /// Max queries per category (default: 100)
    #[serde(default = "d_limit_100")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PositionDistributionParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrandedPerformanceParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Brand terms to match, e.g. ["acme", "acme corp"] (case-insensitive)
    pub brand_terms: Vec<String>,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchAllRowsParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Dimensions to group by (default: ["query"])
    #[serde(default)]
    pub dimensions: Option<Vec<String>>,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Optional filters (same format as search_analytics)
    #[serde(default)]
    pub dimension_filter_groups: Option<Vec<DimensionFilterGroup>>,
    /// Safety ceiling on total rows fetched (default: 200000)
    #[serde(default = "d_max_paginated")]
    pub max_rows: u64,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExportSearchAnalyticsParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Dimensions to group by (default: ["query"])
    #[serde(default)]
    pub dimensions: Option<Vec<String>>,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Optional filters (same format as search_analytics)
    #[serde(default)]
    pub dimension_filter_groups: Option<Vec<DimensionFilterGroup>>,
    /// Safety ceiling on total rows fetched (default: 200000)
    #[serde(default = "d_max_paginated")]
    pub max_rows: u64,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
    /// CSV filename or relative path, resolved inside the export directory
    /// ($GSC_EXPORT_DIR, else ~/Downloads). Absolute paths and '..' are rejected.
    /// Default: gsc-export-<site>-<dates>-<time>.csv
    #[serde(default)]
    pub output_path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CtrOpportunityParams {
    /// The Search Console property URL
    pub site_url: String,
    /// Start date YYYY-MM-DD (default: 28 days ago)
    #[serde(default)]
    pub start_date: Option<String>,
    /// End date YYYY-MM-DD (default: 3 days ago)
    #[serde(default)]
    pub end_date: Option<String>,
    /// Minimum impressions to qualify (default: 100)
    #[serde(default = "d_min_impressions_100")]
    pub min_impressions: u64,
    /// Max rows to return (default: 50)
    #[serde(default = "d_limit_50")]
    pub limit: usize,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AggregateSitesParams {
    /// Properties to include (default: all verified properties in the account)
    #[serde(default)]
    pub site_urls: Option<Vec<String>>,
    /// Number of days per period (default: 28)
    #[serde(default = "d_days_28")]
    pub days: i64,
    /// web, image, video, news, discover, or googleNews
    #[serde(default = "d_web")]
    pub search_type: String,
    /// Sort sites by this current-period metric — clicks or impressions (default: clicks)
    #[serde(default = "d_clicks")]
    pub sort_by: String,
}

// --- Tools ---

#[tool_router]
impl GscServer {
    #[tool(
        description = "List all verified Google Search Console properties with permission levels."
    )]
    async fn list_sites(&self) -> Result<CallToolResult, McpError> {
        to_result(self.state.client.list_sites().await)
    }

    #[tool(
        description = "Query Google Search Console search analytics data. Full-featured search performance query: combine any dimensions to get clicks, impressions, CTR, and position data."
    )]
    async fn search_analytics(
        &self,
        Parameters(p): Parameters<SearchAnalyticsParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let dimensions = p.dimensions.unwrap_or_else(|| vec!["query".to_string()]);
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                dimension_filter_groups: p.dimension_filter_groups,
                row_limit: p.row_limit,
                start_row: p.start_row,
                search_type: p.search_type.clone(),
                aggregation_type: p.aggregation_type,
                data_state: None,
            };
            let rows = execute_search_analytics(state, &q).await?;
            let out: Vec<Value> = rows.iter().map(|r| Value::Object(r.to_map(&dimensions))).collect();
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "dimensions": dimensions,
                "search_type": p.search_type,
                "row_count": out.len(),
                "rows": out,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Compare two date ranges side-by-side for the same dimensions/filters. Returns both periods plus deltas (change in clicks, impressions, CTR, position). Both periods are paginated so deltas and new/lost detection are correct on large properties. If a period's fetch hits max_rows, rows missing from that period are marked data_incomplete with null metrics/deltas (not zero) — treat them as \"not fetched\", never as \"dropped to zero\"."
    )]
    async fn compare_periods(
        &self,
        Parameters(p): Parameters<ComparePeriodsParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let dimensions = p.dimensions.unwrap_or_else(|| vec!["query".to_string()]);
            for (value, name) in [
                (&p.period1_start, "period1_start"),
                (&p.period1_end, "period1_end"),
                (&p.period2_start, "period2_start"),
                (&p.period2_end, "period2_end"),
            ] {
                validate_date(value, name)?;
            }
            let (mut compared, trunc1, trunc2) = compare_periods_by_dimension(
                state,
                &p.site_url,
                &p.period1_start,
                &p.period1_end,
                &p.period2_start,
                &p.period2_end,
                &dimensions,
                p.dimension_filter_groups,
                p.max_rows.min(HARD_MAX_ROWS),
                &p.search_type,
            )
            .await?;
            // Null deltas (data_incomplete rows) sort to the end rather than as zeroes.
            compared.sort_by(|a, b| {
                let k = |m: &JsonMap| {
                    m.get("delta_clicks").and_then(Value::as_f64).unwrap_or(f64::NEG_INFINITY)
                };
                k(b).total_cmp(&k(a))
            });
            let mut result = json!({
                "site_url": p.site_url,
                "period1": format!("{} to {}", p.period1_start, p.period1_end),
                "period2": format!("{} to {}", p.period2_start, p.period2_end),
                "dimensions": dimensions,
                "truncated": trunc1 || trunc2,
                "period1_truncated": trunc1,
                "period2_truncated": trunc2,
                "row_count": compared.len(),
                "rows": compared,
            });
            if trunc1 || trunc2 {
                result["truncation_warning"] = Value::String(
                    "One or both period fetches hit max_rows. Rows marked data_incomplete have \
                     null metrics for the missing period — that period's data was not fetched, \
                     it did NOT necessarily drop to zero. Do not report incomplete rows as \
                     new/lost; re-check them with a filtered standalone query."
                        .into(),
                );
            }
            Ok(result)
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Inspect a URL's indexing status, mobile usability, rich results, and AMP status."
    )]
    async fn inspect_url(
        &self,
        Parameters(p): Parameters<InspectUrlParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let site_url = validate_site_url(&p.site_url)?;
            state.client.inspect_url(&p.url, &site_url, &p.language_code).await
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "List all submitted sitemaps for a Search Console property. Returns sitemap URLs, types, last submitted/downloaded dates, error counts, warning counts, and content type breakdowns."
    )]
    async fn list_sitemaps(
        &self,
        Parameters(p): Parameters<SiteUrlParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let site_url = validate_site_url(&p.site_url)?;
            state.client.list_sitemaps(&site_url).await
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Get the top N search queries for a site. Quick shortcut — no need to configure full search_analytics params."
    )]
    async fn top_queries(
        &self,
        Parameters(p): Parameters<TopNParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            top_by_dimension(
                &self.state,
                "query",
                &p.site_url,
                p.start_date.as_deref(),
                p.end_date.as_deref(),
                p.limit,
                &p.sort_by,
                &p.search_type,
            )
            .await,
        )
    }

    #[tool(
        description = "Get the top N pages for a site. Quick shortcut — no need to configure full search_analytics params."
    )]
    async fn top_pages(
        &self,
        Parameters(p): Parameters<TopNParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            top_by_dimension(
                &self.state,
                "page",
                &p.site_url,
                p.start_date.as_deref(),
                p.end_date.as_deref(),
                p.limit,
                &p.sort_by,
                &p.search_type,
            )
            .await,
        )
    }

    #[tool(
        description = "Combined query+page dimension report — which search terms drive traffic to which pages. Supports filtering by page URL pattern or query pattern. The essential SEO report."
    )]
    async fn query_page_report(
        &self,
        Parameters(p): Parameters<QueryPageReportParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            // Python truthiness: empty-string filters are treated as absent.
            let mut filters = vec![];
            if let Some(pf) = p.page_filter.as_ref().filter(|s| !s.is_empty()) {
                filters.push(("page", pf.clone()));
            }
            if let Some(qf) = p.query_filter.as_ref().filter(|s| !s.is_empty()) {
                filters.push(("query", qf.clone()));
            }
            let dimension_filter_groups = if filters.is_empty() {
                None
            } else {
                Some(vec![DimensionFilterGroup {
                    group_type: None,
                    filters: filters
                        .into_iter()
                        .map(|(dim, expr)| crate::analytics::DimensionFilter {
                            dimension: dim.to_string(),
                            operator: Some("contains".to_string()),
                            expression: expr,
                        })
                        .collect(),
                }])
            };
            let dimensions = vec!["query".to_string(), "page".to_string()];
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                dimension_filter_groups,
                row_limit: p.row_limit,
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let rows = execute_search_analytics(state, &q).await?;
            let out: Vec<Value> = rows.iter().map(|r| Value::Object(r.to_map(&dimensions))).collect();
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "page_filter": p.page_filter,
                "query_filter": p.query_filter,
                "row_count": out.len(),
                "rows": out,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Find queries with high impressions but low CTR in striking distance (positions 4-20). These are quick SEO wins — queries already ranking on page 1-2 that could gain significant traffic with small improvements."
    )]
    async fn quick_wins(
        &self,
        Parameters(p): Parameters<QuickWinsParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let dimensions = vec!["query".to_string(), "page".to_string()];
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let (rows, truncated) = fetch_all_rows(state, &q, MAX_PAGINATED_ROWS).await?;
            let mut wins: Vec<JsonMap> = vec![];
            for row in &rows {
                if row.impressions >= p.min_impressions as f64
                    && p.min_position <= row.position
                    && row.position <= p.max_position
                {
                    // Estimate clicks gained if CTR reached the striking-distance target; the
                    // position-aware expected CTR keeps deep results honest, clamped at 0 so
                    // already-overperforming rows don't show negative "potential".
                    let target_ctr = QUICK_WIN_TARGET_CTR.max(expected_ctr(row.position));
                    let potential = (row.impressions * target_ctr - row.clicks).max(0.0);
                    let mut win = row.to_map(&dimensions);
                    win.insert("potential_clicks".into(), num(round_to(potential, 1)));
                    wins.push(win);
                }
            }
            sort_maps_by(&mut wins, "potential_clicks", true);
            wins.truncate(p.limit);
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "filters": {
                    "min_impressions": p.min_impressions,
                    "min_position": p.min_position,
                    "max_position": p.max_position,
                },
                "truncated": truncated,
                "row_count": wins.len(),
                "rows": wins,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Detect rising and falling search queries by comparing the last N days against the preceding N days."
    )]
    async fn trending_queries(
        &self,
        Parameters(p): Parameters<TrendingQueriesParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (p1s, p1e, p2s, p2e) = resolve_comparison_periods(p.days)?;
            let dims = vec!["query".to_string()];
            let (compared, trunc1, trunc2) = compare_periods_by_dimension(
                state, &p.site_url, &p1s, &p1e, &p2s, &p2e, &dims, None, MAX_PAGINATED_ROWS,
                &p.search_type,
            )
            .await?;
            // data_incomplete rows have unknown deltas — a false zero would fabricate a trend.
            let (compared, excluded) = drop_incomplete_rows(compared);
            let mut filtered: Vec<JsonMap> = compared
                .into_iter()
                .filter(|r| {
                    getf(r, "period1_impressions") >= p.min_impressions as f64
                        || getf(r, "period2_impressions") >= p.min_impressions as f64
                })
                .collect();
            sort_maps_by(&mut filtered, &p.sort_by, true);
            filtered.truncate(p.limit);
            Ok(json!({
                "site_url": p.site_url,
                "period1": format!("{p1s} to {p1e}"),
                "period2": format!("{p2s} to {p2e}"),
                "sort_by": p.sort_by,
                "truncated": trunc1 || trunc2,
                "excluded_incomplete_rows": excluded,
                "row_count": filtered.len(),
                "rows": filtered,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Detect pages losing traffic by comparing recent vs prior period. Surfaces pages where clicks have declined — candidates for content refresh."
    )]
    async fn content_decay(
        &self,
        Parameters(p): Parameters<ContentDecayParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (p1s, p1e, p2s, p2e) = resolve_comparison_periods(p.days)?;
            let dims = vec!["page".to_string()];
            let (compared, trunc1, trunc2) = compare_periods_by_dimension(
                state, &p.site_url, &p1s, &p1e, &p2s, &p2e, &dims, None, MAX_PAGINATED_ROWS,
                &p.search_type,
            )
            .await?;
            // data_incomplete rows would show a false full decline (period2 not fetched, not 0).
            let (compared, excluded) = drop_incomplete_rows(compared);
            let mut decaying: Vec<JsonMap> = compared
                .into_iter()
                .filter(|r| {
                    getf(r, "period1_clicks") >= p.min_previous_clicks as f64
                        && getf(r, "delta_clicks") < 0.0
                })
                .collect();
            sort_maps_by(&mut decaying, "delta_clicks", false);
            decaying.truncate(p.limit);
            Ok(json!({
                "site_url": p.site_url,
                "period1": format!("{p1s} to {p1e}"),
                "period2": format!("{p2s} to {p2e}"),
                "min_previous_clicks": p.min_previous_clicks,
                "truncated": trunc1 || trunc2,
                "excluded_incomplete_rows": excluded,
                "row_count": decaying.len(),
                "rows": decaying,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Detect keyword cannibalization — queries where multiple pages compete for the same term. Returns queries ranking with 2+ pages, sorted by total impressions."
    )]
    async fn cannibalization(
        &self,
        Parameters(p): Parameters<CannibalizationParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: vec!["query".to_string(), "page".to_string()],
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let (rows, truncated) = fetch_all_rows(state, &q, MAX_PAGINATED_ROWS).await?;

            let mut grouped: HashMap<String, Vec<&crate::analytics::Row>> = HashMap::new();
            for row in &rows {
                let query = row.keys.first().cloned().flatten().unwrap_or_default();
                grouped.entry(query).or_default().push(row);
            }

            let mut results: Vec<JsonMap> = vec![];
            for (query, mut pages) in grouped {
                if pages.len() < 2 {
                    continue;
                }
                let total_clicks: f64 = pages.iter().map(|p| p.clicks).sum();
                let total_impressions: f64 = pages.iter().map(|p| p.impressions).sum();
                if total_impressions < p.min_impressions as f64 {
                    continue;
                }
                pages.sort_by(|a, b| b.clicks.total_cmp(&a.clicks));
                let page_entries: Vec<Value> = pages
                    .iter()
                    .map(|pg| {
                        json!({
                            "page": pg.keys.get(1).cloned().flatten(),
                            "clicks": num(pg.clicks),
                            "impressions": num(pg.impressions),
                            "ctr": num(pg.ctr),
                            "position": num(pg.position),
                        })
                    })
                    .collect();
                let mut entry = JsonMap::new();
                entry.insert("query".into(), Value::String(query));
                entry.insert("page_count".into(), num(pages.len() as f64));
                entry.insert("total_clicks".into(), num(total_clicks));
                entry.insert("total_impressions".into(), num(total_impressions));
                entry.insert("pages".into(), Value::Array(page_entries));
                results.push(entry);
            }
            sort_maps_by(&mut results, "total_impressions", true);
            results.truncate(p.limit);
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "min_impressions": p.min_impressions,
                "truncated": truncated,
                "row_count": results.len(),
                "rows": results,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Get clicks, impressions, CTR, and position broken down by device type."
    )]
    async fn device_breakdown(
        &self,
        Parameters(p): Parameters<DeviceBreakdownParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            // Python truthiness: an empty-string page_filter is treated as absent.
            let filters = p
                .page_filter
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|pf| vec![contains_filter("page", pf)]);
            let dimensions = vec!["device".to_string()];
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                dimension_filter_groups: filters,
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let rows = execute_search_analytics(state, &q).await?;
            let out: Vec<Value> = rows.iter().map(|r| Value::Object(r.to_map(&dimensions))).collect();
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "page_filter": p.page_filter,
                "rows": out,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(description = "Get clicks, impressions, CTR, and position broken down by country.")]
    async fn country_breakdown(
        &self,
        Parameters(p): Parameters<CountryBreakdownParams>,
    ) -> Result<CallToolResult, McpError> {
        to_result(
            top_by_dimension(
                &self.state,
                "country",
                &p.site_url,
                p.start_date.as_deref(),
                p.end_date.as_deref(),
                p.limit,
                "clicks",
                &p.search_type,
            )
            .await,
        )
    }

    #[tool(
        description = "Query near-real-time search analytics data including yesterday's partial data. Same as search_analytics but uses dataState=all to include fresh (incomplete) data; end date defaults to yesterday instead of 3 days ago."
    )]
    async fn fresh_data(
        &self,
        Parameters(p): Parameters<FreshDataParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let dimensions = p.dimensions.unwrap_or_else(|| vec!["query".to_string()]);
            let today = chrono::Local::now().date_naive();
            let end_date = match p.end_date.as_deref() {
                Some(s) => validate_date(s, "end_date")?,
                None => (today - chrono::Duration::days(FRESH_DATA_LAG_DAYS))
                    .format("%Y-%m-%d")
                    .to_string(),
            };
            let start_date = match p.start_date.as_deref() {
                Some(s) => validate_date(s, "start_date")?,
                None => (today - chrono::Duration::days(DEFAULT_WINDOW_DAYS))
                    .format("%Y-%m-%d")
                    .to_string(),
            };
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                dimension_filter_groups: p.dimension_filter_groups,
                row_limit: p.row_limit,
                start_row: p.start_row,
                search_type: p.search_type.clone(),
                aggregation_type: p.aggregation_type,
                data_state: Some("all".to_string()),
            };
            let rows = execute_search_analytics(state, &q).await?;
            let out: Vec<Value> = rows.iter().map(|r| Value::Object(r.to_map(&dimensions))).collect();
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "data_state": "all",
                "dimensions": dimensions,
                "search_type": p.search_type,
                "row_count": out.len(),
                "rows": out,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(description = "Submit a sitemap URL to Google Search Console.")]
    async fn submit_sitemap(
        &self,
        Parameters(p): Parameters<SitemapParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let site_url = validate_site_url(&p.site_url)?;
            state.client.submit_sitemap(&site_url, &p.sitemap_url).await?;
            Ok(json!({"site_url": site_url, "sitemap_url": p.sitemap_url, "status": "submitted"}))
        }
        .await;
        to_result(r)
    }

    #[tool(description = "Delete a previously submitted sitemap from Google Search Console.")]
    async fn delete_sitemap(
        &self,
        Parameters(p): Parameters<SitemapParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let site_url = validate_site_url(&p.site_url)?;
            state.client.delete_sitemap(&site_url, &p.sitemap_url).await?;
            Ok(json!({"site_url": site_url, "sitemap_url": p.sitemap_url, "status": "deleted"}))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Inspect multiple URLs for indexing status, mobile usability, and rich results. Inspects each URL individually and returns all results. Note: the URL Inspection API has a quota of ~2000 requests/day, so use judiciously."
    )]
    async fn batch_inspect(
        &self,
        Parameters(p): Parameters<BatchInspectParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let site_url = validate_site_url(&p.site_url)?;
            if p.urls.len() > MAX_BATCH_INSPECT_URLS {
                return Err(GscError::invalid(format!(
                    "batch_inspect accepts at most {MAX_BATCH_INSPECT_URLS} urls per call \
                     (got {}); chunk the list across calls.",
                    p.urls.len()
                )));
            }
            let mut results: Vec<Value> = vec![];
            let mut quota_exhausted = false;
            for url in &p.urls {
                match state.client.inspect_url(url, &site_url, &p.language_code).await {
                    Ok(mut result) => {
                        if let Value::Object(m) = &mut result {
                            m.insert("url".into(), Value::String(url.clone()));
                        }
                        results.push(result);
                    }
                    Err(e) => {
                        results.push(json!({"url": url, "error": e.message()}));
                        // Stop hammering a depleted quota — the daily cap is ~2000 inspections.
                        if e.status() == Some(429) {
                            quota_exhausted = true;
                            break;
                        }
                    }
                }
            }
            Ok(json!({
                "site_url": site_url,
                "urls_requested": p.urls.len(),
                "urls_inspected": results.len(),
                "quota_exhausted": quota_exhausted,
                "results": results,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Get an aggregate performance summary with period-over-period comparison. Returns total clicks, impressions, average CTR, and average position for the current period vs the previous period of equal length, plus deltas."
    )]
    async fn performance_summary(
        &self,
        Parameters(p): Parameters<PerformanceSummaryParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (p1s, p1e, p2s, p2e) = resolve_comparison_periods(p.days)?;
            let previous = aggregate_period(state, &p.site_url, &p1s, &p1e, &p.search_type).await?;
            let current = aggregate_period(state, &p.site_url, &p2s, &p2e, &p.search_type).await?;
            let empty: Vec<String> = vec![];
            Ok(json!({
                "site_url": p.site_url,
                "period1": format!("{p1s} to {p1e}"),
                "period2": format!("{p2s} to {p2e}"),
                "previous": previous.to_map(&empty),
                "current": current.to_map(&empty),
                "deltas": {
                    "clicks": num(current.clicks - previous.clicks),
                    "impressions": num(current.impressions - previous.impressions),
                    "ctr": num(round_to(current.ctr - previous.ctr, CTR_PRECISION)),
                    "position": num(round_to(current.position - previous.position, POSITION_PRECISION)),
                },
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Get daily time-series data for clicks, impressions, CTR, and position. Returns one row per day, ideal for charting trends over time."
    )]
    async fn date_trend(
        &self,
        Parameters(p): Parameters<DateTrendParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let dimensions = vec!["date".to_string()];
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                dimension_filter_groups: p.dimension_filter_groups,
                row_limit: MAX_ROW_LIMIT,
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let rows = execute_search_analytics(state, &q).await?;
            let mut out: Vec<JsonMap> = rows.iter().map(|r| r.to_map(&dimensions)).collect();
            sort_maps_by_str(&mut out, "date");
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "row_count": out.len(),
                "rows": out,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Get performance breakdown by search appearance type (rich results, AMP, video, FAQ, etc). Shows which SERP features your pages appear in and their performance."
    )]
    async fn search_appearance(
        &self,
        Parameters(p): Parameters<SearchAppearanceParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let dimensions = vec!["searchAppearance".to_string()];
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let rows = execute_search_analytics(state, &q).await?;
            let mut out: Vec<JsonMap> = rows.iter().map(|r| r.to_map(&dimensions)).collect();
            sort_maps_by(&mut out, "clicks", true);
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "row_count": out.len(),
                "rows": out,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Get all search queries driving traffic to a specific page, sorted by clicks."
    )]
    async fn landing_page_queries(
        &self,
        Parameters(p): Parameters<LandingPageQueriesParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let dimensions = vec!["query".to_string()];
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                dimension_filter_groups: Some(vec![single_filter("page", "equals", &p.page_url)]),
                row_limit: p.limit as u64,
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let rows = execute_search_analytics(state, &q).await?;
            let mut out: Vec<JsonMap> = rows.iter().map(|r| r.to_map(&dimensions)).collect();
            sort_maps_by(&mut out, "clicks", true);
            Ok(json!({
                "site_url": p.site_url,
                "page_url": p.page_url,
                "start_date": start_date,
                "end_date": end_date,
                "row_count": out.len(),
                "rows": out,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Find pages with the largest traffic growth by comparing recent vs prior period. Like trending_queries but for pages."
    )]
    async fn top_growing_pages(
        &self,
        Parameters(p): Parameters<TopGrowingPagesParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (p1s, p1e, p2s, p2e) = resolve_comparison_periods(p.days)?;
            let dims = vec!["page".to_string()];
            let (compared, trunc1, trunc2) = compare_periods_by_dimension(
                state, &p.site_url, &p1s, &p1e, &p2s, &p2e, &dims, None, MAX_PAGINATED_ROWS,
                &p.search_type,
            )
            .await?;
            // data_incomplete rows would show false full growth (period1 not fetched, not 0).
            let (compared, excluded) = drop_incomplete_rows(compared);
            let mut filtered: Vec<JsonMap> = compared
                .into_iter()
                .filter(|r| {
                    (getf(r, "period1_clicks") >= p.min_clicks as f64
                        || getf(r, "period2_clicks") >= p.min_clicks as f64)
                        && getf(r, "delta_clicks") > 0.0
                })
                .collect();
            sort_maps_by(&mut filtered, "delta_clicks", true);
            filtered.truncate(p.limit);
            Ok(json!({
                "site_url": p.site_url,
                "period1": format!("{p1s} to {p1e}"),
                "period2": format!("{p2s} to {p2e}"),
                "truncated": trunc1 || trunc2,
                "excluded_incomplete_rows": excluded,
                "row_count": filtered.len(),
                "rows": filtered,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Find queries that newly appeared or completely disappeared between two periods. \"New\" queries had zero impressions in the previous period; \"lost\" queries dropped to zero."
    )]
    async fn new_lost_queries(
        &self,
        Parameters(p): Parameters<NewLostQueriesParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (p1s, p1e, p2s, p2e) = resolve_comparison_periods(p.days)?;
            let dims = vec!["query".to_string()];
            let (compared, trunc1, trunc2) = compare_periods_by_dimension(
                state, &p.site_url, &p1s, &p1e, &p2s, &p2e, &dims, None, MAX_PAGINATED_ROWS,
                &p.search_type,
            )
            .await?;
            // data_incomplete rows are exactly the false new/lost candidates — exclude them.
            let (compared, excluded) = drop_incomplete_rows(compared);
            let min = p.min_impressions as f64;
            let mut new_queries: Vec<JsonMap> = compared
                .iter()
                .filter(|r| getf(r, "period1_impressions") == 0.0 && getf(r, "period2_impressions") >= min)
                .cloned()
                .collect();
            let mut lost_queries: Vec<JsonMap> = compared
                .iter()
                .filter(|r| getf(r, "period1_impressions") >= min && getf(r, "period2_impressions") == 0.0)
                .cloned()
                .collect();
            sort_maps_by(&mut new_queries, "period2_impressions", true);
            sort_maps_by(&mut lost_queries, "period1_impressions", true);
            let new_count = new_queries.len();
            let lost_count = lost_queries.len();
            new_queries.truncate(p.limit);
            lost_queries.truncate(p.limit);
            Ok(json!({
                "site_url": p.site_url,
                "period1": format!("{p1s} to {p1e}"),
                "period2": format!("{p2s} to {p2e}"),
                "min_impressions": p.min_impressions,
                "truncated": trunc1 || trunc2,
                "excluded_incomplete_rows": excluded,
                "new_query_count": new_count,
                "lost_query_count": lost_count,
                "new_queries": new_queries,
                "lost_queries": lost_queries,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Analyze the distribution of queries across position ranges (1-3, 4-10, 11-20, 21-50, 50+) with aggregated clicks, impressions, and share percentages per bucket."
    )]
    async fn position_distribution(
        &self,
        Parameters(p): Parameters<PositionDistributionParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: vec!["query".to_string()],
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let (rows, truncated) = fetch_all_rows(state, &q, MAX_PAGINATED_ROWS).await?;

            let mut bucket_data: Vec<(u64, f64, f64)> = vec![(0, 0.0, 0.0); POSITION_BUCKETS.len()];
            for row in &rows {
                for (i, (low, high, _)) in POSITION_BUCKETS.iter().enumerate() {
                    if *low <= row.position && row.position <= *high {
                        bucket_data[i].0 += 1;
                        bucket_data[i].1 += row.clicks;
                        bucket_data[i].2 += row.impressions;
                        break;
                    }
                }
            }
            let total_clicks: f64 = rows.iter().map(|r| r.clicks).sum();
            let total_impressions: f64 = rows.iter().map(|r| r.impressions).sum();

            let buckets: Vec<Value> = POSITION_BUCKETS
                .iter()
                .enumerate()
                .map(|(i, (_, _, label))| {
                    let (count, clicks, impressions) = bucket_data[i];
                    json!({
                        "range": label,
                        "query_count": count,
                        "clicks": num(clicks),
                        "impressions": num(impressions),
                        "click_share": if total_clicks > 0.0 { num(round_to(clicks / total_clicks, CTR_PRECISION)) } else { num(0.0) },
                        "impression_share": if total_impressions > 0.0 { num(round_to(impressions / total_impressions, CTR_PRECISION)) } else { num(0.0) },
                    })
                })
                .collect();

            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "truncated": truncated,
                "total_queries": rows.len(),
                "total_clicks": num(total_clicks),
                "total_impressions": num(total_impressions),
                "buckets": buckets,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Split search performance into branded vs non-branded traffic. Branded queries match any of the provided brand terms (case-insensitive). Returns aggregate metrics for each segment plus the branded traffic share."
    )]
    async fn branded_performance(
        &self,
        Parameters(p): Parameters<BrandedPerformanceParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let terms: Vec<&String> = p.brand_terms.iter().filter(|t| !t.trim().is_empty()).collect();
            if terms.is_empty() {
                return Err(GscError::invalid("brand_terms must contain at least one non-empty term."));
            }
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            // (?i) makes the RE2 match case-insensitive — GSC regex filters are case-sensitive
            // by default, which would silently undercount branded queries like "ACME".
            let regex_pattern = format!(
                "(?i){}",
                terms.iter().map(|t| re2_escape(t)).collect::<Vec<_>>().join("|")
            );

            // Paginate both segments past the 25k per-request cap; a single capped request
            // would silently undercount the aggregate shares on large properties.
            let base = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: vec!["query".to_string()],
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let branded_q = SaQuery {
                dimension_filter_groups: Some(vec![single_filter("query", "includingRegex", &regex_pattern)]),
                ..base.clone()
            };
            let non_branded_q = SaQuery {
                dimension_filter_groups: Some(vec![single_filter("query", "excludingRegex", &regex_pattern)]),
                ..base
            };
            let (branded_rows, branded_trunc) = fetch_all_rows(state, &branded_q, MAX_PAGINATED_ROWS).await?;
            let (non_branded_rows, non_branded_trunc) =
                fetch_all_rows(state, &non_branded_q, MAX_PAGINATED_ROWS).await?;

            let aggregate = |rows: &[crate::analytics::Row]| -> Value {
                let total_clicks: f64 = rows.iter().map(|r| r.clicks).sum();
                let total_impressions: f64 = rows.iter().map(|r| r.impressions).sum();
                let avg_ctr = if total_impressions > 0.0 {
                    round_to(total_clicks / total_impressions, 4)
                } else {
                    0.0
                };
                let avg_position = if total_impressions > 0.0 {
                    round_to(
                        rows.iter().map(|r| r.position * r.impressions).sum::<f64>() / total_impressions,
                        1,
                    )
                } else {
                    0.0
                };
                json!({
                    "clicks": num(total_clicks),
                    "impressions": num(total_impressions),
                    "ctr": num(avg_ctr),
                    "position": num(avg_position),
                    "query_count": rows.len(),
                })
            };

            let branded = aggregate(&branded_rows);
            let non_branded = aggregate(&non_branded_rows);
            let all_clicks = getf(branded.as_object().unwrap(), "clicks")
                + getf(non_branded.as_object().unwrap(), "clicks");
            let all_impressions = getf(branded.as_object().unwrap(), "impressions")
                + getf(non_branded.as_object().unwrap(), "impressions");
            let branded_clicks = getf(branded.as_object().unwrap(), "clicks");
            let branded_impressions = getf(branded.as_object().unwrap(), "impressions");

            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "brand_terms": p.brand_terms,
                "truncated": branded_trunc || non_branded_trunc,
                "branded": branded,
                "non_branded": non_branded,
                "branded_click_share": if all_clicks > 0.0 { num(round_to(branded_clicks / all_clicks, 4)) } else { num(0.0) },
                "branded_impression_share": if all_impressions > 0.0 { num(round_to(branded_impressions / all_impressions, 4)) } else { num(0.0) },
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Fetch all rows the API exposes, paginating past the 25,000-row per-request cap. Loops with startRow until the API stops returning rows or max_rows is reached. Note: Google returns top rows only and never guarantees completeness."
    )]
    async fn fetch_all_rows(
        &self,
        Parameters(p): Parameters<FetchAllRowsParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let dimensions = p.dimensions.unwrap_or_else(|| vec!["query".to_string()]);
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                dimension_filter_groups: p.dimension_filter_groups,
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let (rows, truncated) = fetch_all_rows(state, &q, p.max_rows.min(HARD_MAX_ROWS)).await?;
            let out: Vec<Value> = rows.iter().map(|r| Value::Object(r.to_map(&dimensions))).collect();
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "dimensions": dimensions,
                "search_type": p.search_type,
                "truncated": truncated,
                "row_count": out.len(),
                "rows": out,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Export all rows the API exposes to a CSV file on disk instead of the chat. Paginates past the 25,000-row API cap with concurrent page fetches, writes <dimensions>,clicks,impressions,ctr,position columns, and returns the file path plus a small summary. Use this for bulk pulls that the response size cap would otherwise truncate; analyze the file with local tools."
    )]
    async fn export_search_analytics(
        &self,
        Parameters(p): Parameters<ExportSearchAnalyticsParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let dimensions = p.dimensions.unwrap_or_else(|| vec!["query".to_string()]);
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                dimension_filter_groups: p.dimension_filter_groups,
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let (rows, truncated) =
                fetch_all_rows_concurrent(state, &q, p.max_rows.min(HARD_MAX_ROWS), 4).await?;
            let (total_clicks, total_impressions) = rows
                .iter()
                .fold((0.0, 0.0), |(c, i), r| (c + r.clicks, i + r.impressions));
            let csv = rows_to_csv(&rows, &dimensions);
            let root = export_root();
            let path = match p.output_path {
                // output_path is model-supplied and therefore untrusted (prompt-injected
                // data can steer it) — confine it to the export root, no escapes.
                Some(ref custom) => root.join(validate_export_rel_path(custom)?),
                None => root.join(format!(
                    "gsc-export-{}-{}-{}-{}.csv",
                    filename_fragment(&p.site_url),
                    start_date,
                    end_date,
                    chrono::Local::now().format("%H%M%S"),
                )),
            };
            let bytes = csv.len();
            let write_path = path.clone();
            tokio::task::spawn_blocking(move || write_export_confined(&root, &write_path, csv.as_bytes()))
                .await
                .map_err(|e| GscError::Api {
                    status: None,
                    raw: format!("export write task failed: {e}"),
                })??;
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "dimensions": dimensions,
                "search_type": p.search_type,
                "path": path.display().to_string(),
                "file_size_bytes": bytes,
                "row_count": rows.len(),
                "truncated": truncated,
                "total_clicks": num(total_clicks),
                "total_impressions": num(total_impressions),
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Find queries underperforming their expected CTR for their ranking position. Ranks by estimated extra clicks available if each query reached the position-based benchmark CTR — surfaces title/snippet optimization opportunities at any position."
    )]
    async fn ctr_opportunity(
        &self,
        Parameters(p): Parameters<CtrOpportunityParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            let (start_date, end_date) = resolve_dates(p.start_date.as_deref(), p.end_date.as_deref())?;
            let dimensions = vec!["query".to_string()];
            let q = SaQuery {
                site_url: p.site_url.clone(),
                start_date: start_date.clone(),
                end_date: end_date.clone(),
                dimensions: dimensions.clone(),
                search_type: p.search_type.clone(),
                ..Default::default()
            };
            let (rows, truncated) = fetch_all_rows(state, &q, MAX_PAGINATED_ROWS).await?;
            let mut opportunities: Vec<JsonMap> = vec![];
            for row in &rows {
                if row.impressions < p.min_impressions as f64 {
                    continue;
                }
                let expected = expected_ctr(row.position);
                if row.ctr >= expected {
                    continue;
                }
                let gap_clicks = round_to((expected - row.ctr) * row.impressions, 1);
                if gap_clicks <= 0.0 {
                    continue;
                }
                let mut entry = row.to_map(&dimensions);
                entry.insert("expected_ctr".into(), num(expected));
                entry.insert("ctr_gap".into(), num(round_to(expected - row.ctr, CTR_PRECISION)));
                entry.insert("potential_clicks".into(), num(gap_clicks));
                opportunities.push(entry);
            }
            sort_maps_by(&mut opportunities, "potential_clicks", true);
            opportunities.truncate(p.limit);
            Ok(json!({
                "site_url": p.site_url,
                "start_date": start_date,
                "end_date": end_date,
                "min_impressions": p.min_impressions,
                "truncated": truncated,
                "row_count": opportunities.len(),
                "rows": opportunities,
            }))
        }
        .await;
        to_result(r)
    }

    #[tool(
        description = "Roll up performance across multiple properties with period-over-period deltas — a portfolio view for agencies and multi-brand owners. If site_urls is omitted, every verified property in the account is included; a failure on one site is reported inline without aborting the rest."
    )]
    async fn aggregate_sites(
        &self,
        Parameters(p): Parameters<AggregateSitesParams>,
    ) -> Result<CallToolResult, McpError> {
        let state = &self.state;
        let r = async {
            if p.sort_by != "clicks" && p.sort_by != "impressions" {
                return Err(GscError::invalid(format!(
                    "Invalid sort_by '{}'; choose 'clicks' or 'impressions'.",
                    p.sort_by
                )));
            }
            let site_urls: Vec<String> = match p.site_urls {
                Some(urls) => urls,
                None => {
                    let listed = state.client.list_sites().await?;
                    listed
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter(|s| {
                                    s.get("permissionLevel").and_then(Value::as_str)
                                        != Some("siteUnverifiedUser")
                                })
                                .filter_map(|s| s.get("siteUrl").and_then(Value::as_str))
                                .map(|s| s.to_string())
                                .collect()
                        })
                        .unwrap_or_default()
                }
            };
            let (p1s, p1e, p2s, p2e) = resolve_comparison_periods(p.days)?;

            let mut sites: Vec<JsonMap> = vec![];
            let (mut prev_clicks, mut prev_impr, mut curr_clicks, mut curr_impr) = (0.0, 0.0, 0.0, 0.0);
            for site_url in &site_urls {
                let fetched = async {
                    let previous = aggregate_period(state, site_url, &p1s, &p1e, &p.search_type).await?;
                    let current = aggregate_period(state, site_url, &p2s, &p2e, &p.search_type).await?;
                    Ok::<_, GscError>((previous, current))
                }
                .await;
                match fetched {
                    Ok((previous, current)) => {
                        prev_clicks += previous.clicks;
                        prev_impr += previous.impressions;
                        curr_clicks += current.clicks;
                        curr_impr += current.impressions;
                        let empty: Vec<String> = vec![];
                        let mut entry = JsonMap::new();
                        entry.insert("site_url".into(), Value::String(site_url.clone()));
                        entry.insert("previous".into(), Value::Object(previous.to_map(&empty)));
                        entry.insert("current".into(), Value::Object(current.to_map(&empty)));
                        entry.insert(
                            "deltas".into(),
                            json!({
                                "clicks": num(current.clicks - previous.clicks),
                                "impressions": num(current.impressions - previous.impressions),
                            }),
                        );
                        sites.push(entry);
                    }
                    // Auth failures abort the whole tool; per-site API/input errors are
                    // reported inline (mirrors the Python HttpError/ValueError catch).
                    Err(GscError::Auth(m)) => return Err(GscError::Auth(m)),
                    Err(e) => {
                        let mut entry = JsonMap::new();
                        entry.insert("site_url".into(), Value::String(site_url.clone()));
                        entry.insert("error".into(), Value::String(e.message()));
                        sites.push(entry);
                    }
                }
            }
            sites.sort_by(|a, b| {
                let get = |m: &JsonMap| {
                    m.get("current")
                        .and_then(Value::as_object)
                        .map(|c| getf(c, &p.sort_by))
                        .unwrap_or(0.0)
                };
                get(b).total_cmp(&get(a))
            });
            let error_count = sites.iter().filter(|s| s.contains_key("error")).count();
            Ok(json!({
                "period1": format!("{p1s} to {p1e}"),
                "period2": format!("{p2s} to {p2e}"),
                "site_count": site_urls.len(),
                // portfolio_totals sum only the successful sites; error_count flags the gap.
                "succeeded_count": site_urls.len() - error_count,
                "error_count": error_count,
                "portfolio_totals": {
                    "previous": {"clicks": num(prev_clicks), "impressions": num(prev_impr)},
                    "current": {"clicks": num(curr_clicks), "impressions": num(curr_impr)},
                    "deltas": {
                        "clicks": num(curr_clicks - prev_clicks),
                        "impressions": num(curr_impr - prev_impr),
                    },
                },
                "sites": sites,
            }))
        }
        .await;
        to_result(r)
    }
}

// --- Prompt argument structs ---

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SeoAuditArgs {
    /// The Search Console property URL
    pub site_url: String,
    /// Comma-separated brand terms (optional)
    #[serde(default)]
    pub brand_terms: Option<String>,
    /// Number of days per period (default: 28)
    #[serde(default)]
    pub days: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SiteDaysArgs {
    /// The Search Console property URL
    pub site_url: String,
    /// Number of days per period (default: 28)
    #[serde(default)]
    pub days: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrafficDiagnosisArgs {
    /// The Search Console property URL
    pub site_url: String,
    /// Comma-separated brand terms (optional)
    #[serde(default)]
    pub brand_terms: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageDeepDiveArgs {
    /// The Search Console property URL
    pub site_url: String,
    /// The page URL to analyze
    pub page_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SiteOnlyArgs {
    /// The Search Console property URL
    pub site_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InternationalAuditArgs {
    /// The Search Console property URL
    pub site_url: String,
    /// Number of top countries to analyze (default: 10)
    #[serde(default)]
    pub top_n_countries: Option<String>,
}

fn parse_u32(s: &Option<String>, default: u32) -> u32 {
    s.as_deref().and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

fn parse_terms(s: &Option<String>) -> Option<Vec<String>> {
    let terms: Vec<String> = s
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() { None } else { Some(terms) }
}

fn user_prompt(description: &str, text: String) -> GetPromptResult {
    GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)])
        .with_description(description)
}

// --- Prompts ---

#[prompt_router]
impl GscServer {
    #[prompt(
        name = "seo_audit",
        description = "Comprehensive SEO audit — runs multiple tools and synthesizes findings"
    )]
    async fn seo_audit(
        &self,
        Parameters(a): Parameters<SeoAuditArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let terms = parse_terms(&a.brand_terms);
        let text = prompts::seo_audit(&a.site_url, terms.as_deref(), parse_u32(&a.days, 28));
        Ok(user_prompt("Full SEO audit workflow for a site.", text))
    }

    #[prompt(
        name = "content_opportunities",
        description = "Find content optimization opportunities — quick wins, trending queries, new keywords"
    )]
    async fn content_opportunities(
        &self,
        Parameters(a): Parameters<SiteDaysArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let text = prompts::content_opportunities(&a.site_url, parse_u32(&a.days, 28));
        Ok(user_prompt("Discover content optimization opportunities.", text))
    }

    #[prompt(
        name = "traffic_diagnosis",
        description = "Diagnose traffic drops — identify causes and recommend fixes"
    )]
    async fn traffic_diagnosis(
        &self,
        Parameters(a): Parameters<TrafficDiagnosisArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let terms = parse_terms(&a.brand_terms);
        let text = prompts::traffic_diagnosis(&a.site_url, terms.as_deref());
        Ok(user_prompt("Diagnose why traffic may be dropping.", text))
    }

    #[prompt(
        name = "page_deep_dive",
        description = "Deep-dive analysis of a specific page's SEO performance"
    )]
    async fn page_deep_dive(
        &self,
        Parameters(a): Parameters<PageDeepDiveArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let text = prompts::page_deep_dive(&a.site_url, &a.page_url);
        Ok(user_prompt("Analyze a specific page's search performance in depth.", text))
    }

    #[prompt(name = "weekly_report", description = "Generate a weekly SEO performance report")]
    async fn weekly_report(
        &self,
        Parameters(a): Parameters<SiteOnlyArgs>,
    ) -> Result<GetPromptResult, McpError> {
        Ok(user_prompt(
            "Weekly SEO performance summary.",
            prompts::weekly_report(&a.site_url),
        ))
    }

    #[prompt(
        name = "international_audit",
        description = "Analyze international SEO performance by country"
    )]
    async fn international_audit(
        &self,
        Parameters(a): Parameters<InternationalAuditArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let text = prompts::international_audit(&a.site_url, parse_u32(&a.top_n_countries, 10));
        Ok(user_prompt("Analyze search performance across countries.", text))
    }

    #[prompt(
        name = "technical_health",
        description = "Technical SEO health check — indexing, sitemaps, mobile usability"
    )]
    async fn technical_health(
        &self,
        Parameters(a): Parameters<SiteOnlyArgs>,
    ) -> Result<GetPromptResult, McpError> {
        Ok(user_prompt(
            "Check technical SEO health: indexing, sitemaps, mobile usability.",
            prompts::technical_health(&a.site_url),
        ))
    }
}

// --- Resources & server info ---

const RES_SITES: &str = "gsc://sites";
const TPL_SITEMAPS: &str = "gsc://site/{site_url}/sitemaps";
const TPL_SUMMARY: &str = "gsc://site/{site_url}/summary";

impl GscServer {
    async fn resource_sites(&self) -> Result<String, GscError> {
        let sites = self.state.client.list_sites().await?;
        Ok(serde_json::to_string_pretty(&sites).unwrap_or_else(|_| "[]".to_string()))
    }

    async fn resource_sitemaps(&self, site_url: &str) -> Result<String, GscError> {
        let sitemaps = self.state.client.list_sitemaps(site_url).await?;
        Ok(serde_json::to_string_pretty(&sitemaps).unwrap_or_else(|_| "[]".to_string()))
    }

    async fn resource_summary(&self, site_url: &str) -> Result<String, GscError> {
        let (p1s, p1e, p2s, p2e) = resolve_comparison_periods(28)?;
        let previous = aggregate_period(&self.state, site_url, &p1s, &p1e, "web").await?;
        let current = aggregate_period(&self.state, site_url, &p2s, &p2e, "web").await?;
        let empty: Vec<String> = vec![];
        let v = json!({
            "site_url": site_url,
            "period1": format!("{p1s} to {p1e}"),
            "period2": format!("{p2s} to {p2e}"),
            "previous": previous.to_map(&empty),
            "current": current.to_map(&empty),
            "deltas": {
                "clicks": num(current.clicks - previous.clicks),
                "impressions": num(current.impressions - previous.impressions),
                "ctr": num(round_to(current.ctr - previous.ctr, 4)),
                "position": num(round_to(current.position - previous.position, 1)),
            },
        });
        Ok(serde_json::to_string_pretty(&v).unwrap_or_default())
    }
}

fn decode_uri_component(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().to_string()
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for GscServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        );
        info.server_info.name = "Google Search Console".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(
            "MCP server exposing Google Search Console data: search analytics, SEO analysis \
             (quick wins, cannibalization, CTR opportunities, trends), URL inspection, and \
             sitemap management. Property URLs are either 'https://example.com/' (trailing \
             slash required) or 'sc-domain:example.com'."
                .to_string(),
        );
        info
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let mut sites = Resource::new(RES_SITES, "sites")
            .with_description("All verified Google Search Console properties");
        sites.mime_type = Some("application/json".to_string());
        Ok(ListResourcesResult::with_all_items(vec![sites]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let mut sitemaps = ResourceTemplate::new(TPL_SITEMAPS, "site_sitemaps")
            .with_description("Sitemaps submitted for a GSC property");
        sitemaps.mime_type = Some("application/json".to_string());
        let mut summary = ResourceTemplate::new(TPL_SUMMARY, "site_summary")
            .with_description("28-day performance summary for a GSC property");
        summary.mime_type = Some("application/json".to_string());
        Ok(ListResourceTemplatesResult::with_all_items(vec![sitemaps, summary]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = request.uri.as_str();
        let text = if uri == RES_SITES {
            self.resource_sites().await
        } else if let Some(rest) = uri.strip_prefix("gsc://site/") {
            if let Some(site) = rest.strip_suffix("/sitemaps") {
                self.resource_sitemaps(&decode_uri_component(site)).await
            } else if let Some(site) = rest.strip_suffix("/summary") {
                self.resource_summary(&decode_uri_component(site)).await
            } else {
                return Err(McpError::resource_not_found(
                    format!("Unknown resource URI: {uri}"),
                    None,
                ));
            }
        } else {
            return Err(McpError::resource_not_found(
                format!("Unknown resource URI: {uri}"),
                None,
            ));
        };
        match text {
            Ok(body) => Ok(ReadResourceResult::new(vec![ResourceContents::text(body, uri)])),
            Err(e) => Err(McpError::internal_error(e.message(), None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_rel_path_accepts_plain_csv_paths() {
        for value in ["report.csv", "sub/dir/report.CSV", "a.b.csv"] {
            assert!(validate_export_rel_path(value).is_ok(), "should accept {value:?}");
        }
    }

    #[test]
    fn export_rel_path_rejects_escapes_streams_and_devices() {
        for value in [
            "../out.csv",
            "/tmp/out.csv",
            "report.csv:hidden",
            "existing.txt:x.csv",
            r"C:\windows\out.csv",
            "NUL.csv",
            "con.csv",
            "sub/Lpt9.csv",
            "report.txt",
        ] {
            assert!(validate_export_rel_path(value).is_err(), "should reject {value:?}");
        }
    }
}
