//! Core Search Analytics plumbing shared by every analytics tool: input validation,
//! date-window resolution, the single query executor with its short-TTL response cache,
//! pagination past the 25k per-request cap, period comparison, and top-N shortcuts.
//! Faithful port of the Python server's helper layer.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{Duration, Local, NaiveDate};
use lru::LruCache;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::GscError;
use crate::gsc::GscClient;

pub type JsonMap = Map<String, Value>;

// --- Tunable constants (mirrors the Python module constants) ---
pub const MAX_ROW_LIMIT: u64 = 25000; // GSC Search Analytics API per-request cap
pub const DEFAULT_ROW_LIMIT: u64 = 1000; // default page size for general queries
pub const DATA_LAG_DAYS: i64 = 3; // GSC finalizes data ~3 days back
pub const FRESH_DATA_LAG_DAYS: i64 = 1; // fresh_data intentionally queries closer to "today"
pub const DEFAULT_WINDOW_DAYS: i64 = 28; // default analysis window
pub const QUICK_WIN_TARGET_CTR: f64 = 0.15; // assumed achievable CTR for striking-distance queries
pub const CTR_PRECISION: i32 = 4; // decimal places for CTR values
pub const POSITION_PRECISION: i32 = 1; // decimal places for average position values
/// Cap for paginated full-corpus fetches (8 pages of 25k). Bounds runaway loops.
pub const MAX_PAGINATED_ROWS: u64 = MAX_ROW_LIMIT * 8;
pub const DEFAULT_TAIL_CTR: f64 = 0.01; // expected CTR beyond position 10
pub const RESPONSE_CACHE_TTL_SECS: u64 = 300; // seconds to reuse an identical response
pub const RESPONSE_CACHE_MAX: usize = 256; // max distinct cached responses (LRU eviction)

pub const VALID_SORT_FIELDS: [&str; 4] = ["clicks", "impressions", "ctr", "position"];

/// Bounds are contiguous (each high == the next low) so every average position — which
/// is almost always fractional — lands in exactly one bucket; the first match wins so the
/// shared integer boundary (e.g. 10.0) is not double-counted. Lower bound 0 captures
/// missing/sub-1.0 positions, keeping per-bucket shares summing to the totals.
pub const POSITION_BUCKETS: [(f64, f64, &str); 5] = [
    (0.0, 3.0, "1-3"),
    (3.0, 10.0, "4-10"),
    (10.0, 20.0, "11-20"),
    (20.0, 50.0, "21-50"),
    (50.0, f64::INFINITY, "50+"),
];

/// Approximate organic CTR by average position — used for opportunity scoring.
pub const EXPECTED_CTR_BY_POSITION: [(i64, f64); 10] = [
    (1, 0.28),
    (2, 0.15),
    (3, 0.11),
    (4, 0.08),
    (5, 0.07),
    (6, 0.05),
    (7, 0.04),
    (8, 0.03),
    (9, 0.025),
    (10, 0.02),
];

// --- Numeric helpers ---

/// Round to `digits` decimal places (half away from zero — differs from Python's
/// banker's rounding only on exact decimal ties, which real CTR/position data
/// essentially never produces).
pub fn round_to(x: f64, digits: i32) -> f64 {
    let m = 10f64.powi(digits);
    (x * m).round() / m
}

/// Python's `int(round(x))`: nearest integer, ties to even.
pub fn py_round_i64(x: f64) -> i64 {
    let floor = x.floor();
    let diff = x - floor;
    if diff > 0.5 {
        floor as i64 + 1
    } else if diff < 0.5 {
        floor as i64
    } else {
        let f = floor as i64;
        if f % 2 == 0 { f } else { f + 1 }
    }
}

/// JSON number that stays an integer when whole (so clicks serialize as `5`, not `5.0`).
pub fn num(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_992.0 {
        Value::Number(serde_json::Number::from(x as i64))
    } else {
        serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
    }
}

pub fn expected_ctr(position: f64) -> f64 {
    let rounded = py_round_i64(position);
    for (pos, ctr) in EXPECTED_CTR_BY_POSITION {
        if rounded <= pos {
            return ctr;
        }
    }
    DEFAULT_TAIL_CTR
}

/// Read a numeric field from a result map, defaulting to 0 (Python `row.get(k, 0)`).
pub fn getf(map: &JsonMap, key: &str) -> f64 {
    map.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Stable sort of result maps by a numeric field.
pub fn sort_maps_by(rows: &mut [JsonMap], key: &str, descending: bool) {
    if descending {
        rows.sort_by(|a, b| getf(b, key).total_cmp(&getf(a, key)));
    } else {
        rows.sort_by(|a, b| getf(a, key).total_cmp(&getf(b, key)));
    }
}

/// Stable sort of result maps by a string field (used by date_trend).
pub fn sort_maps_by_str(rows: &mut [JsonMap], key: &str) {
    rows.sort_by(|a, b| {
        let av = a.get(key).and_then(Value::as_str).unwrap_or_default();
        let bv = b.get(key).and_then(Value::as_str).unwrap_or_default();
        av.cmp(bv)
    });
}

// --- Input validation & date resolution ---

/// Validate/normalize a GSC property URL, erroring on clearly-wrong formats.
/// GSC silently returns empty results for malformed site_urls, so fail loud instead.
pub fn validate_site_url(site_url: &str) -> Result<String, GscError> {
    let s = site_url.trim();
    if s.is_empty() {
        return Err(GscError::invalid("site_url is required"));
    }
    if let Some(domain) = s.strip_prefix("sc-domain:") {
        if domain.is_empty() || domain.contains('/') || !domain.contains('.') {
            return Err(GscError::invalid(format!(
                "Invalid domain property '{site_url}'; expected 'sc-domain:example.com'."
            )));
        }
        return Ok(s.to_string());
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return Ok(if s.ends_with('/') { s.to_string() } else { format!("{s}/") });
    }
    Err(GscError::invalid(format!(
        "Invalid site_url '{site_url}'. Use a URL-prefix property like 'https://example.com/' \
         (with trailing slash) or a domain property like 'sc-domain:example.com'."
    )))
}

pub fn validate_date(value: &str, field: &str) -> Result<String, GscError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| GscError::invalid(format!("{field} must be in YYYY-MM-DD format, got '{value}'")))?;
    Ok(value.to_string())
}

fn today_local() -> NaiveDate {
    Local::now().date_naive()
}

/// Default window: 28 days ending 3 days ago (GSC data lag). Both endpoints are
/// inclusive, so the start is `DEFAULT_WINDOW_DAYS - 1` days back — subtracting the
/// full window would span 29 dates.
pub fn resolve_dates(
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> Result<(String, String), GscError> {
    let end_default = today_local() - Duration::days(DATA_LAG_DAYS);
    let start_default = end_default - Duration::days(DEFAULT_WINDOW_DAYS - 1);
    let start_out = match start_date {
        Some(s) => validate_date(s, "start_date")?,
        None => start_default.format("%Y-%m-%d").to_string(),
    };
    let end_out = match end_date {
        Some(s) => validate_date(s, "end_date")?,
        None => end_default.format("%Y-%m-%d").to_string(),
    };
    if start_out > end_out {
        // ISO dates compare lexicographically == chronologically
        return Err(GscError::invalid(format!(
            "start_date ({start_out}) must be on or before end_date ({end_out})"
        )));
    }
    Ok((start_out, end_out))
}

/// GSC retains ~16 months of data; this generous ceiling exists to keep untrusted
/// `days` values out of chrono's Duration arithmetic (which panics on overflow).
pub const MAX_PERIOD_DAYS: i64 = 3650;

/// Two contiguous, non-overlapping periods of `days` each, ending at the data lag.
pub fn resolve_comparison_periods(days: i64) -> Result<(String, String, String, String), GscError> {
    if days < 1 {
        return Err(GscError::invalid(format!("days must be >= 1, got {days}")));
    }
    if days > MAX_PERIOD_DAYS {
        return Err(GscError::invalid(format!(
            "days must be <= {MAX_PERIOD_DAYS}, got {days} (GSC retains ~16 months of data)"
        )));
    }
    let end = today_local() - Duration::days(DATA_LAG_DAYS);
    let fmt = |d: NaiveDate| d.format("%Y-%m-%d").to_string();
    let period2_end = fmt(end);
    let period2_start = fmt(end - Duration::days(days - 1));
    let period1_end = fmt(end - Duration::days(days));
    let period1_start = fmt(end - Duration::days(2 * days - 1));
    Ok((period1_start, period1_end, period2_start, period2_end))
}

// --- Query & row types ---

/// One dimension filter, e.g. `{"dimension": "query", "operator": "contains", "expression": "python"}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DimensionFilter {
    /// query, page, country, device, or searchAppearance
    pub dimension: String,
    /// contains, equals, notContains, notEquals, includingRegex, or excludingRegex
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    pub expression: String,
}

/// A filter group; filters within a group are ANDed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DimensionFilterGroup {
    #[serde(default, rename = "groupType", alias = "group_type", skip_serializing_if = "Option::is_none")]
    pub group_type: Option<String>,
    pub filters: Vec<DimensionFilter>,
}

pub fn contains_filter(dimension: &str, expression: &str) -> DimensionFilterGroup {
    DimensionFilterGroup {
        group_type: None,
        filters: vec![DimensionFilter {
            dimension: dimension.to_string(),
            operator: Some("contains".to_string()),
            expression: expression.to_string(),
        }],
    }
}

pub fn single_filter(dimension: &str, operator: &str, expression: &str) -> DimensionFilterGroup {
    DimensionFilterGroup {
        group_type: None,
        filters: vec![DimensionFilter {
            dimension: dimension.to_string(),
            operator: Some(operator.to_string()),
            expression: expression.to_string(),
        }],
    }
}

/// One formatted search-analytics row (Python `_format_row` output).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub keys: Vec<Option<String>>,
    pub clicks: f64,
    pub impressions: f64,
    pub ctr: f64,      // rounded to CTR_PRECISION
    pub position: f64, // rounded to POSITION_PRECISION
}

impl Row {
    pub fn key_for(&self, dimensions: &[String]) -> Vec<Option<String>> {
        (0..dimensions.len()).map(|i| self.keys.get(i).cloned().flatten()).collect()
    }

    pub fn to_map(&self, dimensions: &[String]) -> JsonMap {
        let mut m = JsonMap::new();
        for (i, dim) in dimensions.iter().enumerate() {
            let v = match self.keys.get(i).cloned().flatten() {
                Some(k) => Value::String(k),
                None => Value::Null,
            };
            m.insert(dim.clone(), v);
        }
        m.insert("clicks".into(), num(self.clicks));
        m.insert("impressions".into(), num(self.impressions));
        m.insert("ctr".into(), num(self.ctr));
        m.insert("position".into(), num(self.position));
        m
    }
}

fn format_row(raw: &Value) -> Row {
    let keys = raw
        .get("keys")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|k| k.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let f = |k: &str| raw.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    Row {
        keys,
        clicks: f("clicks"),
        impressions: f("impressions"),
        ctr: round_to(f("ctr"), CTR_PRECISION),
        position: round_to(f("position"), POSITION_PRECISION),
    }
}

/// Parameters for one searchanalytics.query call.
#[derive(Debug, Clone)]
pub struct SaQuery {
    pub site_url: String,
    pub start_date: String,
    pub end_date: String,
    pub dimensions: Vec<String>,
    pub dimension_filter_groups: Option<Vec<DimensionFilterGroup>>,
    pub row_limit: u64,
    pub start_row: u64,
    pub search_type: String,
    pub aggregation_type: String,
    pub data_state: Option<String>,
}

impl Default for SaQuery {
    fn default() -> Self {
        SaQuery {
            site_url: String::new(),
            start_date: String::new(),
            end_date: String::new(),
            dimensions: vec![],
            dimension_filter_groups: None,
            row_limit: DEFAULT_ROW_LIMIT,
            start_row: 0,
            search_type: "web".to_string(),
            aggregation_type: "auto".to_string(),
            data_state: None,
        }
    }
}

// --- App state (client + response cache) ---

pub struct AppState {
    pub client: GscClient,
    /// Short-TTL cache of search-analytics responses keyed on the full request.
    /// The SEO prompt workflows call the same queries repeatedly; this collapses
    /// those into one API hit. Never caches fresh (data_state) responses.
    cache: Mutex<LruCache<String, (Instant, Vec<Row>)>>,
}

impl AppState {
    pub fn new(client: GscClient) -> Self {
        AppState {
            client,
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(RESPONSE_CACHE_MAX).unwrap())),
        }
    }
}

/// Execute one searchanalytics.query call (validating site_url, capping the row limit,
/// consulting the response cache) and return formatted rows.
pub async fn execute_search_analytics(state: &AppState, q: &SaQuery) -> Result<Vec<Row>, GscError> {
    let site_url = validate_site_url(&q.site_url)?;
    let capped_limit = q.row_limit.min(MAX_ROW_LIMIT);
    // Python truthiness: an empty filter list is treated as no filters at all
    // (same API request, same cache entry).
    let filter_groups = q.dimension_filter_groups.as_ref().filter(|g| !g.is_empty());

    // Never cache fresh/real-time data — it changes between calls by design.
    let cache_key = if q.data_state.is_none() {
        let key = serde_json::to_string(&serde_json::json!([
            site_url,
            q.start_date,
            q.end_date,
            q.dimensions,
            filter_groups,
            capped_limit,
            q.start_row,
            q.search_type,
            q.aggregation_type,
        ]))
        .unwrap_or_default();
        let mut cache = state.cache.lock().unwrap();
        if let Some((stored_at, rows)) = cache.get(&key) {
            if stored_at.elapsed().as_secs() < RESPONSE_CACHE_TTL_SECS {
                return Ok(rows.clone());
            }
        }
        Some(key)
    } else {
        None
    };

    let mut body = serde_json::json!({
        "startDate": q.start_date,
        "endDate": q.end_date,
        "dimensions": q.dimensions,
        "rowLimit": capped_limit,
        "startRow": q.start_row,
        "type": q.search_type,
        "aggregationType": q.aggregation_type,
    });
    if let Some(groups) = filter_groups {
        body["dimensionFilterGroups"] = serde_json::to_value(groups)
            .map_err(|e| GscError::invalid(format!("bad dimension_filter_groups: {e}")))?;
    }
    if let Some(ds) = &q.data_state {
        body["dataState"] = Value::String(ds.clone());
    }

    let response = state.client.query_search_analytics(&site_url, &body).await?;
    let rows: Vec<Row> = response
        .get("rows")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(format_row).collect())
        .unwrap_or_default();

    if let Some(key) = cache_key {
        let mut cache = state.cache.lock().unwrap();
        cache.put(key, (Instant::now(), rows.clone()));
    }
    Ok(rows)
}

/// Page through search analytics beyond the 25k per-request cap, driven by a fetch
/// closure `(start_row, page_size) -> rows` so the pagination logic is unit-testable.
/// Returns (rows, truncated): `truncated` means the local `max_rows` cap was hit before
/// the API stopped returning pages.
///
/// `truncated == false` is NOT a completeness guarantee. Search Analytics returns top
/// rows only and caps at ~50k rows per day per search type, so exhausting pagination
/// proves the API stopped serving rows, not that the corpus is complete.
pub async fn fetch_all_rows_with<F, Fut>(mut fetch: F, max_rows: u64) -> Result<(Vec<Row>, bool), GscError>
where
    F: FnMut(u64, u64) -> Fut,
    Fut: Future<Output = Result<Vec<Row>, GscError>>,
{
    let mut all_rows: Vec<Row> = Vec::new();
    let mut start_row: u64 = 0;
    while (all_rows.len() as u64) < max_rows {
        let page_size = MAX_ROW_LIMIT.min(max_rows - all_rows.len() as u64);
        let page = fetch(start_row, page_size).await?;
        let page_len = page.len() as u64;
        all_rows.extend(page);
        if page_len < page_size {
            return Ok((all_rows, false)); // API exhausted before the ceiling
        }
        start_row += page_len;
    }
    // Hit the max_rows ceiling on a full page. Probe one row past it to distinguish
    // "corpus exactly filled the cap" (complete) from "more rows remain" (truncated).
    let probe = fetch(start_row, 1).await?;
    Ok((all_rows, !probe.is_empty()))
}

/// Production pagination: wraps `execute_search_analytics` over `base` (whose
/// row_limit/start_row are overridden per page). Same caveat as `fetch_all_rows_with`:
/// `truncated` reports the local `max_rows` cap, and a false there does not mean every
/// row that ever existed was returned.
pub async fn fetch_all_rows(
    state: &AppState,
    base: &SaQuery,
    max_rows: u64,
) -> Result<(Vec<Row>, bool), GscError> {
    fetch_all_rows_with(
        |start_row, page_size| {
            let q = SaQuery {
                row_limit: page_size,
                start_row,
                ..base.clone()
            };
            async move { execute_search_analytics(state, &q).await }
        },
        max_rows,
    )
    .await
}

// --- Period comparison ---

fn rows_to_map(rows: Vec<Row>, dimensions: &[String]) -> HashMap<Vec<Option<String>>, Row> {
    rows.into_iter().map(|r| (r.key_for(dimensions), r)).collect()
}

/// Merge two periods' rows on the dimension key and compute deltas (pure; testable).
///
/// A key absent from a period whose fetch hit `max_rows` (`trunc1`/`trunc2`) may simply
/// have ranked past the local cap. Such one-sided rows get `data_incomplete` set to the
/// missing period's name and `null` (not 0) for that period's metrics and all deltas — a
/// zero here would read as "dropped to zero" when the truth is "not fetched".
///
/// A key absent from a non-truncated period is treated as zero, but that is only zero
/// *as far as the API exposes*: Search Analytics returns top rows only and caps at ~50k
/// rows per day per search type, so it never guarantees the full corpus. Deltas and the
/// new/lost classifications built on them are best-effort; a specific key is verifiable
/// with a targeted filtered query for that key.
pub fn merge_compare(
    rows_p1: Vec<Row>,
    rows_p2: Vec<Row>,
    dimensions: &[String],
    trunc1: bool,
    trunc2: bool,
) -> Vec<JsonMap> {
    let p1_map = rows_to_map(rows_p1, dimensions);
    let p2_map = rows_to_map(rows_p2, dimensions);
    let keys: HashSet<Vec<Option<String>>> = p1_map.keys().chain(p2_map.keys()).cloned().collect();

    let mut compared = Vec::with_capacity(keys.len());
    for key in keys {
        let r1 = p1_map.get(&key);
        let r2 = p2_map.get(&key);
        let p1_unknown = r1.is_none() && trunc1;
        let p2_unknown = r2.is_none() && trunc2;
        let mut entry = JsonMap::new();
        for (dim, k) in dimensions.iter().zip(key.iter()) {
            entry.insert(
                dim.clone(),
                match k {
                    Some(s) => Value::String(s.clone()),
                    None => Value::Null,
                },
            );
        }
        let m = |r: Option<&Row>, f: fn(&Row) -> f64| r.map(f).unwrap_or(0.0);
        let p1_clicks = m(r1, |r| r.clicks);
        let p1_impr = m(r1, |r| r.impressions);
        let p1_ctr = m(r1, |r| r.ctr);
        let p1_pos = m(r1, |r| r.position);
        let p2_clicks = m(r2, |r| r.clicks);
        let p2_impr = m(r2, |r| r.impressions);
        let p2_ctr = m(r2, |r| r.ctr);
        let p2_pos = m(r2, |r| r.position);
        entry.insert("period1_clicks".into(), num(p1_clicks));
        entry.insert("period1_impressions".into(), num(p1_impr));
        entry.insert("period1_ctr".into(), num(p1_ctr));
        entry.insert("period1_position".into(), num(p1_pos));
        entry.insert("period2_clicks".into(), num(p2_clicks));
        entry.insert("period2_impressions".into(), num(p2_impr));
        entry.insert("period2_ctr".into(), num(p2_ctr));
        entry.insert("period2_position".into(), num(p2_pos));
        if p1_unknown || p2_unknown {
            let side = if p1_unknown { "period1" } else { "period2" };
            for field in ["clicks", "impressions", "ctr", "position"] {
                entry.insert(format!("{side}_{field}"), Value::Null);
            }
            entry.insert("delta_clicks".into(), Value::Null);
            entry.insert("delta_impressions".into(), Value::Null);
            entry.insert("delta_ctr".into(), Value::Null);
            entry.insert("delta_position".into(), Value::Null);
            entry.insert("data_incomplete".into(), Value::String(side.into()));
        } else {
            entry.insert("delta_clicks".into(), num(p2_clicks - p1_clicks));
            entry.insert("delta_impressions".into(), num(p2_impr - p1_impr));
            entry.insert("delta_ctr".into(), num(round_to(p2_ctr - p1_ctr, CTR_PRECISION)));
            entry.insert("delta_position".into(), num(round_to(p2_pos - p1_pos, POSITION_PRECISION)));
        }
        compared.push(entry);
    }
    compared
}

/// Remove `data_incomplete` rows before new/lost/trend classification.
///
/// An incomplete row's missing side was never fetched (its period hit max_rows), so
/// treating it as zero would fabricate a "went to zero" or "brand new" finding.
/// Returns (complete_rows, excluded_count) so tools can surface how many were dropped.
pub fn drop_incomplete_rows(compared: Vec<JsonMap>) -> (Vec<JsonMap>, usize) {
    let total = compared.len();
    let complete: Vec<JsonMap> =
        compared.into_iter().filter(|r| !r.contains_key("data_incomplete")).collect();
    let excluded = total - complete.len();
    (complete, excluded)
}

/// Fetch two periods (paginated up to `max_rows`), merge on the dimension key, compute
/// deltas. Returns (compared, period1_truncated, period2_truncated), where `truncated`
/// means that period hit the local `max_rows` cap — see `merge_compare` for how its
/// one-sided rows are marked.
///
/// Pagination widens coverage but does not prove completeness: the API serves top rows
/// only and caps at ~50k rows per day per search type, so "new"/"lost" here means "new
/// or lost among the rows the API exposed". Treat those classifications as best-effort
/// and confirm an individual key with a targeted filtered query before acting on it.
#[allow(clippy::too_many_arguments)]
pub async fn compare_periods_by_dimension(
    state: &AppState,
    site_url: &str,
    period1_start: &str,
    period1_end: &str,
    period2_start: &str,
    period2_end: &str,
    dimensions: &[String],
    dimension_filter_groups: Option<Vec<DimensionFilterGroup>>,
    max_rows: u64,
    search_type: &str,
) -> Result<(Vec<JsonMap>, bool, bool), GscError> {
    let base = SaQuery {
        site_url: site_url.to_string(),
        dimensions: dimensions.to_vec(),
        dimension_filter_groups,
        search_type: search_type.to_string(),
        ..Default::default()
    };
    let q1 = SaQuery {
        start_date: period1_start.to_string(),
        end_date: period1_end.to_string(),
        ..base.clone()
    };
    let q2 = SaQuery {
        start_date: period2_start.to_string(),
        end_date: period2_end.to_string(),
        ..base
    };
    let (rows_p1, trunc1) = fetch_all_rows(state, &q1, max_rows).await?;
    let (rows_p2, trunc2) = fetch_all_rows(state, &q2, max_rows).await?;
    Ok((merge_compare(rows_p1, rows_p2, dimensions, trunc1, trunc2), trunc1, trunc2))
}

// --- Top-N shortcut ---

fn metric(row: &Row, field: &str) -> f64 {
    match field {
        "clicks" => row.clicks,
        "impressions" => row.impressions,
        "ctr" => row.ctr,
        "position" => row.position,
        _ => 0.0,
    }
}

/// Shared implementation for top_queries/top_pages/country_breakdown. Over-fetches when
/// `sort_by != "clicks"` because the API's native order is clicks-desc — sorting a
/// clicks-truncated slice by another field would yield the wrong top-N.
#[allow(clippy::too_many_arguments)]
pub async fn top_by_dimension(
    state: &AppState,
    dimension: &str,
    site_url: &str,
    start_date: Option<&str>,
    end_date: Option<&str>,
    limit: usize,
    sort_by: &str,
    search_type: &str,
) -> Result<Value, GscError> {
    if !VALID_SORT_FIELDS.contains(&sort_by) {
        let mut sorted = VALID_SORT_FIELDS.to_vec();
        sorted.sort();
        return Err(GscError::invalid(format!(
            "Invalid sort_by '{sort_by}'; choose one of {sorted:?}."
        )));
    }
    let (start_date, end_date) = resolve_dates(start_date, end_date)?;
    let fetch_limit = if sort_by == "clicks" { limit as u64 } else { MAX_ROW_LIMIT };
    let dimensions = vec![dimension.to_string()];
    let q = SaQuery {
        site_url: site_url.to_string(),
        start_date: start_date.clone(),
        end_date: end_date.clone(),
        dimensions: dimensions.clone(),
        row_limit: fetch_limit,
        search_type: search_type.to_string(),
        ..Default::default()
    };
    let mut rows = execute_search_analytics(state, &q).await?;
    // Position sorts ascending — rank 1 is the best result. Everything else descending.
    let descending = sort_by != "position";
    if descending {
        rows.sort_by(|a, b| metric(b, sort_by).total_cmp(&metric(a, sort_by)));
    } else {
        rows.sort_by(|a, b| metric(a, sort_by).total_cmp(&metric(b, sort_by)));
    }
    rows.truncate(limit);
    let out: Vec<Value> = rows.iter().map(|r| Value::Object(r.to_map(&dimensions))).collect();
    Ok(serde_json::json!({
        "site_url": site_url,
        "start_date": start_date,
        "end_date": end_date,
        "sort_by": sort_by,
        "row_count": out.len(),
        "rows": out,
    }))
}

/// Aggregate (no-dimension) query for one period — returns the single totals row or zeros.
pub async fn aggregate_period(
    state: &AppState,
    site_url: &str,
    start_date: &str,
    end_date: &str,
    search_type: &str,
) -> Result<Row, GscError> {
    let q = SaQuery {
        site_url: site_url.to_string(),
        start_date: start_date.to_string(),
        end_date: end_date.to_string(),
        dimensions: vec![],
        search_type: search_type.to_string(),
        ..Default::default()
    };
    let rows = execute_search_analytics(state, &q).await?;
    Ok(rows.into_iter().next().unwrap_or(Row {
        keys: vec![],
        clicks: 0.0,
        impressions: 0.0,
        ctr: 0.0,
        position: 0.0,
    }))
}

/// Plan one wave of concurrent page requests: contiguous (start_row, page_size) pairs
/// beginning at `next_start`, capped so the wave never plans past `max_rows` total
/// (given `fetched` rows already collected). Pure, so the boundary math is testable.
pub fn wave_plan(next_start: u64, fetched: u64, max_rows: u64, concurrency: usize) -> Vec<(u64, u64)> {
    let mut plan = Vec::new();
    let mut start = next_start;
    let mut planned = fetched;
    for _ in 0..concurrency {
        if planned >= max_rows {
            break;
        }
        let size = MAX_ROW_LIMIT.min(max_rows - planned);
        plan.push((start, size));
        start += size;
        planned += size;
    }
    plan
}

/// Concurrent full-corpus pagination: pages are requested `concurrency` at a time with
/// fixed startRows and reassembled in order. Semantics match `fetch_all_rows` (returns
/// `(rows, truncated)` with the exact-cap probe), but wall-clock is ~concurrency× better
/// on large corpora. A short page ends the corpus; later pages in that wave (necessarily
/// empty) are discarded.
pub async fn fetch_all_rows_concurrent(
    state: &Arc<AppState>,
    base: &SaQuery,
    max_rows: u64,
    concurrency: usize,
) -> Result<(Vec<Row>, bool), GscError> {
    let concurrency = concurrency.max(1);
    let mut all_rows: Vec<Row> = Vec::new();
    let mut next_start: u64 = 0;
    loop {
        let plan = wave_plan(next_start, all_rows.len() as u64, max_rows, concurrency);
        if plan.is_empty() {
            break;
        }
        let mut handles = Vec::with_capacity(plan.len());
        for &(start_row, page_size) in &plan {
            let st = Arc::clone(state);
            let q = SaQuery { row_limit: page_size, start_row, ..base.clone() };
            handles.push(tokio::spawn(async move { execute_search_analytics(&st, &q).await }));
        }
        for (handle, &(_, page_size)) in handles.into_iter().zip(&plan) {
            let page = handle
                .await
                .map_err(|e| GscError::Api { status: None, raw: format!("export task failed: {e}") })??;
            let page_len = page.len() as u64;
            all_rows.extend(page);
            if page_len < page_size {
                return Ok((all_rows, false)); // API exhausted before the ceiling
            }
            next_start += page_len;
        }
    }
    // Hit the max_rows ceiling on full pages — probe one row past to distinguish
    // "corpus exactly filled the cap" from "more rows remain" (same as fetch_all_rows).
    let probe_q = SaQuery { row_limit: 1, start_row: next_start, ..base.clone() };
    let probe = execute_search_analytics(state, &probe_q).await?;
    Ok((all_rows, !probe.is_empty()))
}

/// Escape one CSV field per RFC 4180: quote when it contains a comma, quote, or newline.
pub fn csv_escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// Neutralize a spreadsheet formula: a field starting with '=', '+', '-' or '@' is
/// executed as a formula by Excel/Sheets/LibreOffice, so prefix a single quote (the
/// standard mitigation) before the field is CSV-escaped. Dimension values only —
/// search queries are attacker-controllable, the metric columns are our own numbers.
fn csv_escape_dimension(field: &str) -> String {
    if field.starts_with(['=', '+', '-', '@']) {
        csv_escape(&format!("'{field}"))
    } else {
        csv_escape(field)
    }
}

/// Render rows as CSV: dimension columns in order, then clicks/impressions/ctr/position.
/// Dimension values go through `csv_escape_dimension`, which also neutralizes formula
/// injection (a query like `=cmd|...` would otherwise execute when the file is opened).
pub fn rows_to_csv(rows: &[Row], dimensions: &[String]) -> String {
    let mut out = String::new();
    let header: Vec<String> = dimensions
        .iter()
        .map(|d| csv_escape(d))
        .chain(["clicks", "impressions", "ctr", "position"].iter().map(|s| s.to_string()))
        .collect();
    out.push_str(&header.join(","));
    out.push('\n');
    for row in rows {
        let mut fields: Vec<String> = (0..dimensions.len())
            .map(|i| {
                csv_escape_dimension(row.keys.get(i).cloned().flatten().unwrap_or_default().as_str())
            })
            .collect();
        fields.push(num(row.clicks).to_string());
        fields.push(num(row.impressions).to_string());
        fields.push(num(row.ctr).to_string());
        fields.push(num(row.position).to_string());
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

/// Sanitize a site URL into a filename fragment: alphanumerics kept, runs of anything
/// else collapsed to single dashes ("https://www.example.com/" -> "www-example-com").
pub fn filename_fragment(site_url: &str) -> String {
    let stripped = site_url
        .trim_start_matches("sc-domain:")
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let mut out = String::new();
    for c in stripped.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Byte budget for a single tool response (Python `MAX_RESPONSE_BYTES`). MCP clients
/// can't tell the server their context budget, and an oversized result either floods
/// the chat or gets spilled to a file the model must grep — so the server self-limits.
/// `GSC_MAX_RESPONSE_BYTES` overrides; 0 disables the guard.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 120_000;

fn response_byte_budget() -> usize {
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("GSC_MAX_RESPONSE_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES)
    })
}

/// Trim an oversized tool result so it stays chat-sized (port of the Python server's
/// `_truncate_for_chat`). Finds the largest top-level array in an object result and
/// keeps only as many rows as fit the byte budget, flagging the cut with
/// `response_truncated` and a guidance note. Deliberately a distinct flag from
/// `truncated` (which several tools set to mean the upstream FETCH hit max_rows) —
/// overwriting that would conflate two different failure modes. When even the trimmed
/// candidate stays over budget (one giant row, another oversized field, or no array to
/// trim at all) the result degrades to the `omitted_result` fallback rather than shipping
/// an oversized payload. Non-object results and anything under budget pass through
/// untouched. Size is measured compact, matching rmcp's wire serialization (the Python
/// server measures indented for the same reason — FastMCP pretty-prints).
pub fn truncate_for_chat(value: Value) -> Value {
    truncate_for_chat_with(value, response_byte_budget())
}

pub fn truncate_for_chat_with(value: Value, budget: usize) -> Value {
    if budget == 0 {
        return value;
    }
    let Value::Object(ref map) = value else { return value };
    let wire_len = |v: &Value| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
    let size = wire_len(&value);
    if size <= budget {
        return value;
    }
    let Some(key) = map
        .iter()
        .filter(|(_, v)| v.as_array().is_some_and(|a| !a.is_empty()))
        .max_by_key(|(_, v)| wire_len(v))
        .map(|(k, _)| k.clone())
    else {
        // Oversized, but nothing trimmable — drop the bulk instead of passing it through.
        return omitted_result(map, None, 0);
    };
    let rows = map[&key].as_array().cloned().unwrap_or_default();
    let rows_size = wire_len(&map[&key]);
    let overhead = size - rows_size + 400; // headroom for the truncation fields below
    let avg_row = (rows_size / rows.len()).max(1);
    let mut keep = if budget > overhead {
        ((budget - overhead) / avg_row).clamp(1, rows.len())
    } else {
        1
    };
    let total = rows.len();
    // The keep estimate assumes average-sized rows; verify the real payload and shrink
    // until it actually fits (uniform rows can overshoot the estimate by a few percent).
    let candidate = if keep >= total {
        // Nothing to trim (a single oversized row) — the fallback below is the only fit.
        Value::Object(map.clone())
    } else {
        loop {
            let mut out = map.clone();
            out.insert(key.clone(), Value::Array(rows[..keep].to_vec()));
            if key == "rows" && out.contains_key("row_count") {
                out.insert("row_count".to_string(), Value::from(keep as u64));
            }
            out.insert("response_truncated".to_string(), Value::Bool(true));
            out.insert(
                "truncation_note".to_string(),
                Value::String(format!(
                    "Result trimmed from {total} to {keep} {key} to fit the response size limit. \
                     Only the head of the result's existing sort order is shown — everything ranked \
                     past {keep} was dropped. Beware: in a delta-sorted comparison (e.g. \
                     compare_periods) the dropped tail holds the largest declines. Narrow the query \
                     (filters, shorter date range, lower row_limit) or paginate with start_row."
                )),
            );
            let candidate = Value::Object(out);
            if keep == 1 || wire_len(&candidate) <= budget {
                break candidate;
            }
            keep -= (keep / 10).max(1);
        }
    };
    // Trimming to one row still isn't a guarantee: that row (or an unrelated huge field)
    // can blow the budget on its own. Only ship a candidate that actually fits.
    if wire_len(&candidate) <= budget {
        return candidate;
    }
    omitted_result(map, Some(key.as_str()), total)
}

/// Last-resort payload when trimming can't get a result under budget: keep the small
/// scalar/metadata fields, empty out the offending array, and point at the ways to get
/// the data anyway. Always tiny, so the budget guard can't be defeated by one huge row.
fn omitted_result(map: &JsonMap, key: Option<&str>, total: usize) -> Value {
    // Metadata is expected to be short; anything bigger is part of the bulk.
    const MAX_KEPT_FIELD_BYTES: usize = 1_000;
    let wire_len = |v: &Value| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
    let mut out = JsonMap::new();
    for (k, v) in map {
        if Some(k.as_str()) == key {
            out.insert(k.clone(), Value::Array(vec![]));
        } else if !v.is_array() && wire_len(v) <= MAX_KEPT_FIELD_BYTES {
            out.insert(k.clone(), v.clone());
        }
    }
    if key == Some("rows") && out.contains_key("row_count") {
        out.insert("row_count".to_string(), Value::from(0u64));
    }
    let dropped = match key {
        Some(k) => format!("all {total} {k} were omitted"),
        None => "the oversized fields were omitted".to_string(),
    };
    out.insert("response_truncated".to_string(), Value::Bool(true));
    out.insert(
        "truncation_note".to_string(),
        Value::String(format!(
            "Result too large to return inline even after trimming, so {dropped}. Narrow the \
             query (filters, shorter date range, fewer dimensions, lower row_limit) or use \
             export_search_analytics to write the full result to a file."
        )),
    );
    Value::Object(out)
}

/// Escape RE2 metacharacters (the GSC regex dialect) in a literal term.
pub fn re2_escape(term: &str) -> String {
    let mut out = String::with_capacity(term.len() * 2);
    for c in term.chars() {
        if matches!(c, '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- export helpers (wave_plan / csv / filename) ---

    #[test]
    fn wave_plan_contiguous_and_capped() {
        // 60k corpus cap, 4-wide waves: 25k + 25k + 10k, boundaries contiguous.
        let plan = wave_plan(0, 0, 60_000, 4);
        assert_eq!(plan, vec![(0, 25_000), (25_000, 25_000), (50_000, 10_000)]);
        // Second wave resumes where the first ended and respects rows already fetched.
        let plan = wave_plan(50_000, 50_000, 60_000, 4);
        assert_eq!(plan, vec![(50_000, 10_000)]);
        // Nothing left to plan.
        assert!(wave_plan(60_000, 60_000, 60_000, 4).is_empty());
    }

    #[test]
    fn csv_escape_quotes_specials() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_escape("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn rows_to_csv_shapes_header_and_rows() {
        let rows = vec![
            Row { keys: vec![Some("seo, agency".into()), Some("https://x.com/a".into())],
                  clicks: 5.0, impressions: 100.0, ctr: 0.05, position: 3.2 },
            Row { keys: vec![None, Some("https://x.com/b".into())],
                  clicks: 0.0, impressions: 10.0, ctr: 0.0, position: 12.0 },
        ];
        let dims = vec!["query".to_string(), "page".to_string()];
        let csv = rows_to_csv(&rows, &dims);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "query,page,clicks,impressions,ctr,position");
        assert_eq!(lines[1], "\"seo, agency\",https://x.com/a,5,100,0.05,3.2");
        assert_eq!(lines[2], ",https://x.com/b,0,10,0,12");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn rows_to_csv_neutralizes_formula_injection() {
        let rows = vec![
            Row { keys: vec![Some("=cmd|' /C calc'!A0".into())],
                  clicks: 1.0, impressions: 2.0, ctr: 0.5, position: 1.0 },
            Row { keys: vec![Some("-2+3".into())], clicks: 1.0, impressions: 2.0, ctr: 0.5, position: 1.0 },
            Row { keys: vec![Some("@SUM(A1)".into())], clicks: 1.0, impressions: 2.0, ctr: 0.5, position: 1.0 },
            Row { keys: vec![Some("+1,2".into())], clicks: 1.0, impressions: 2.0, ctr: 0.5, position: 1.0 },
        ];
        let csv = rows_to_csv(&rows, &["query".to_string()]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[1], "'=cmd|' /C calc'!A0,1,2,0.5,1");
        assert_eq!(lines[2], "'-2+3,1,2,0.5,1");
        assert_eq!(lines[3], "'@SUM(A1),1,2,0.5,1");
        // quoting still applies on top of the neutralizing prefix
        assert_eq!(lines[4], "\"'+1,2\",1,2,0.5,1");
        // negative metrics are ours, not attacker input — left alone
        let neg = rows_to_csv(
            &[Row { keys: vec![Some("ok".into())], clicks: 0.0, impressions: 0.0, ctr: 0.0, position: -1.0 }],
            &["query".to_string()],
        );
        assert_eq!(neg.lines().nth(1).unwrap(), "ok,0,0,0,-1");
    }

    #[test]
    fn filename_fragment_sanitizes() {
        assert_eq!(filename_fragment("https://www.example.com/"), "www-example-com");
        assert_eq!(filename_fragment("sc-domain:my-site.co.uk"), "my-site-co-uk");
    }

    // --- truncate_for_chat (response size guard) ---

    fn big_rows(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                serde_json::json!({
                    "query": format!("some long search query string number {i}"),
                    "clicks": i, "impressions": i * 10, "ctr": 0.0123, "position": 12.3,
                })
            })
            .collect()
    }

    #[test]
    fn truncate_passes_small_results_untouched() {
        let v = serde_json::json!({"site_url": "https://x.com/", "row_count": 3, "rows": big_rows(3)});
        assert_eq!(truncate_for_chat_with(v.clone(), 120_000), v);
    }

    #[test]
    fn truncate_trims_oversized_rows() {
        let v = serde_json::json!({"site_url": "https://x.com/", "row_count": 5000, "rows": big_rows(5000)});
        let out = truncate_for_chat_with(v.clone(), 10_000);
        assert_eq!(out["response_truncated"], Value::Bool(true));
        // the fetch-truncation flag is a distinct field and must not be set here
        assert!(out.get("truncated").is_none());
        assert!(out["truncation_note"].is_string());
        let kept = out["rows"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 5000);
        assert_eq!(out["row_count"], Value::from(kept as u64));
        // keeps the head, in order
        assert_eq!(out["rows"].as_array().unwrap()[..], v["rows"].as_array().unwrap()[..kept]);
        assert!(serde_json::to_string(&out).unwrap().len() <= 10_000);
    }

    #[test]
    fn truncate_trims_largest_list_key() {
        let v = serde_json::json!({
            "summary": {"clicks": 1}, "small": [1, 2, 3], "opportunities": big_rows(500)
        });
        let out = truncate_for_chat_with(v, 5_000);
        assert_eq!(out["response_truncated"], Value::Bool(true));
        assert!(out["opportunities"].as_array().unwrap().len() < 500);
        assert_eq!(out["small"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn truncate_falls_back_when_single_row_is_over_budget() {
        // One row too big to trim: the old code returned it unchanged, blowing the budget.
        let row = serde_json::json!({ "query": "x".repeat(5_000), "clicks": 1 });
        let v = serde_json::json!({"site_url": "https://x.com/", "row_count": 1, "rows": [row]});
        let out = truncate_for_chat_with(v, 2_000);
        assert_eq!(out["response_truncated"], Value::Bool(true));
        assert_eq!(out["rows"], serde_json::json!([]));
        assert_eq!(out["row_count"], Value::from(0u64));
        assert_eq!(out["site_url"], "https://x.com/"); // small metadata survives
        assert!(out["truncation_note"].as_str().unwrap().contains("export_search_analytics"));
        assert!(serde_json::to_string(&out).unwrap().len() <= 2_000);
    }

    #[test]
    fn truncate_falls_back_when_trimmed_result_still_over_budget() {
        // Trimming the largest array can't help while another huge field remains.
        let v = serde_json::json!({
            "site_url": "https://x.com/", "notes": "y".repeat(8_000), "rows": big_rows(50)
        });
        let out = truncate_for_chat_with(v, 3_000);
        assert_eq!(out["response_truncated"], Value::Bool(true));
        assert_eq!(out["rows"], serde_json::json!([]));
        assert!(out.get("notes").is_none()); // the other oversized field is dropped too
        assert_eq!(out["site_url"], "https://x.com/");
        assert!(serde_json::to_string(&out).unwrap().len() <= 3_000);
    }

    #[test]
    fn truncate_object_without_arrays_is_not_passed_through_oversized() {
        let v = serde_json::json!({"site_url": "https://x.com/", "blob": "z".repeat(9_000)});
        let out = truncate_for_chat_with(v, 1_500);
        assert_eq!(out["response_truncated"], Value::Bool(true));
        assert!(out.get("blob").is_none());
        assert_eq!(out["site_url"], "https://x.com/");
        assert!(serde_json::to_string(&out).unwrap().len() <= 1_500);
    }

    #[test]
    fn truncate_disabled_with_zero_budget() {
        let v = serde_json::json!({"rows": big_rows(5000)});
        assert_eq!(truncate_for_chat_with(v.clone(), 0), v);
    }

    #[test]
    fn truncate_ignores_non_object() {
        let v = Value::Array(big_rows(5000));
        assert_eq!(truncate_for_chat_with(v.clone(), 100), v);
    }

    // --- date math ---

    #[test]
    fn resolve_comparison_periods_contiguous_non_overlapping() {
        let (p1s, p1e, p2s, p2e) = resolve_comparison_periods(7).unwrap();
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!((d(&p1e) - d(&p1s)).num_days(), 6);
        assert_eq!((d(&p2e) - d(&p2s)).num_days(), 6);
        assert_eq!((d(&p2s) - d(&p1e)).num_days(), 1);
        assert!(d(&p1e) < d(&p2s));
    }

    #[test]
    fn resolve_comparison_periods_rejects_zero_days() {
        assert!(resolve_comparison_periods(0).is_err());
    }

    #[test]
    fn resolve_dates_defaults_have_lag() {
        let (start, end) = resolve_dates(None, None).unwrap();
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        // both endpoints are inclusive, so the window covers DEFAULT_WINDOW_DAYS dates
        assert_eq!((d(&end) - d(&start)).num_days() + 1, DEFAULT_WINDOW_DAYS);
        assert_eq!((today_local() - d(&end)).num_days(), DATA_LAG_DAYS);
    }

    #[test]
    fn resolve_dates_rejects_bad_format() {
        assert!(resolve_dates(Some("2024/01/01"), None).is_err());
    }

    #[test]
    fn resolve_dates_rejects_inverted_range() {
        assert!(resolve_dates(Some("2024-02-01"), Some("2024-01-01")).is_err());
    }

    // --- site_url validation ---

    #[test]
    fn validate_site_url_accepts() {
        for (value, expected) in [
            ("sc-domain:example.com", "sc-domain:example.com"),
            ("https://example.com/", "https://example.com/"),
            ("https://example.com", "https://example.com/"), // auto trailing slash
            ("  https://example.com/blog/  ", "https://example.com/blog/"),
        ] {
            assert_eq!(validate_site_url(value).unwrap(), expected);
        }
    }

    #[test]
    fn validate_site_url_rejects() {
        for value in ["example.com", "sc-domain:", "sc-domain:notadomain", ""] {
            assert!(validate_site_url(value).is_err(), "should reject {value:?}");
        }
    }

    // --- row formatting & mapping ---

    #[test]
    fn format_row_handles_short_and_missing_keys() {
        let raw = serde_json::json!({
            "keys": ["python"], "clicks": 5, "impressions": 100, "ctr": 0.05123, "position": 4.567
        });
        let row = format_row(&raw);
        let dims = vec!["query".to_string(), "page".to_string()];
        let out = row.to_map(&dims);
        assert_eq!(out.get("query").unwrap(), "python");
        assert!(out.get("page").unwrap().is_null());
        assert_eq!(out.get("ctr").unwrap().as_f64().unwrap(), 0.0512);
        assert_eq!(out.get("position").unwrap().as_f64().unwrap(), 4.6);
        assert_eq!(out.get("clicks").unwrap().as_i64().unwrap(), 5);
    }

    // --- expected CTR curve ---

    #[test]
    fn expected_ctr_curve() {
        for (pos, ctr) in [(1.0, 0.28), (1.4, 0.28), (3.0, 0.11), (10.0, 0.02), (25.0, 0.01)] {
            assert_eq!(expected_ctr(pos), ctr, "position {pos}");
        }
    }

    #[test]
    fn py_round_ties_to_even() {
        assert_eq!(py_round_i64(2.5), 2);
        assert_eq!(py_round_i64(3.5), 4);
        assert_eq!(py_round_i64(2.4), 2);
        assert_eq!(py_round_i64(2.6), 3);
    }

    // --- position bucketing ---

    fn assign_bucket(pos: f64) -> Option<&'static str> {
        POSITION_BUCKETS
            .iter()
            .find(|(low, high, _)| *low <= pos && pos <= *high)
            .map(|(_, _, label)| *label)
    }

    #[test]
    fn position_buckets_cover_integer_positions() {
        for pos in 1..=100 {
            assert!(assign_bucket(pos as f64).is_some(), "position {pos} fell through");
        }
    }

    #[test]
    fn position_buckets_cover_fractional_positions() {
        for pos in [0.0, 0.4, 0.9, 1.5, 3.0, 3.5, 3.9, 10.0, 10.4, 20.0, 20.7, 50.0, 50.6, 51.0] {
            assert!(assign_bucket(pos).is_some(), "position {pos} fell through");
        }
    }

    #[test]
    fn position_buckets_integer_boundaries_resolve_to_lower() {
        assert_eq!(assign_bucket(3.0), Some("1-3"));
        assert_eq!(assign_bucket(10.0), Some("4-10"));
        assert_eq!(assign_bucket(20.0), Some("11-20"));
        assert_eq!(assign_bucket(50.0), Some("21-50"));
    }

    // --- pagination (with stubbed fetch) ---

    fn mk_rows(n: u64) -> Vec<Row> {
        (0..n)
            .map(|i| Row {
                keys: vec![Some(i.to_string())],
                clicks: 0.0,
                impressions: 0.0,
                ctr: 0.0,
                position: 0.0,
            })
            .collect()
    }

    #[tokio::test]
    async fn fetch_all_rows_paginates_and_flags_truncation() {
        // one full page then a short page => complete corpus, not truncated
        let calls = std::cell::RefCell::new(vec![]);
        let (rows, truncated) = fetch_all_rows_with(
            |start_row, page_size| {
                calls.borrow_mut().push((start_row, page_size));
                async move {
                    Ok(if start_row == 0 { mk_rows(MAX_ROW_LIMIT) } else { mk_rows(1) })
                }
            },
            MAX_PAGINATED_ROWS,
        )
        .await
        .unwrap();
        assert_eq!(rows.len() as u64, MAX_ROW_LIMIT + 1);
        assert!(!truncated);
        let calls = calls.into_inner();
        assert_eq!(calls[0].0, 0);
        assert_eq!(calls[1].0, MAX_ROW_LIMIT);
    }

    #[tokio::test]
    async fn fetch_all_rows_respects_max_rows() {
        // bottomless corpus: always a full page (and the probe finds more)
        let (rows, truncated) =
            fetch_all_rows_with(|_start, page_size| async move { Ok(mk_rows(page_size)) }, 10)
                .await
                .unwrap();
        assert_eq!(rows.len(), 10);
        assert!(truncated);
    }

    #[tokio::test]
    async fn fetch_all_rows_not_truncated_when_corpus_equals_max_rows() {
        let corpus: u64 = 10;
        let (rows, truncated) = fetch_all_rows_with(
            |start_row, page_size| async move {
                let available = corpus.saturating_sub(start_row);
                Ok(mk_rows(page_size.min(available)))
            },
            corpus,
        )
        .await
        .unwrap();
        assert_eq!(rows.len() as u64, corpus);
        assert!(!truncated);
    }

    // --- comparison merge ---

    #[test]
    fn compare_periods_merges_on_dimension() {
        let dims = vec!["query".to_string()];
        let p1 = vec![
            Row { keys: vec![Some("shared".into())], clicks: 10.0, impressions: 100.0, ctr: 0.1, position: 5.0 },
            Row { keys: vec![Some("only1".into())], clicks: 4.0, impressions: 40.0, ctr: 0.1, position: 8.0 },
        ];
        let p2 = vec![Row {
            keys: vec![Some("shared".into())],
            clicks: 20.0,
            impressions: 150.0,
            ctr: 0.13,
            position: 4.0,
        }];
        let out = merge_compare(p1, p2, &dims, false, false);
        let by_q: HashMap<String, &JsonMap> = out
            .iter()
            .map(|m| (m.get("query").unwrap().as_str().unwrap().to_string(), m))
            .collect();
        assert_eq!(getf(by_q["shared"], "delta_clicks"), 10.0);
        assert_eq!(getf(by_q["only1"], "period2_clicks"), 0.0);
        assert_eq!(getf(by_q["only1"], "delta_clicks"), -4.0);
        assert!(!by_q["only1"].contains_key("data_incomplete"));
    }

    #[test]
    fn merge_compare_marks_one_sided_rows_incomplete_when_period_truncated() {
        let dims = vec!["query".to_string()];
        let p1 = vec![
            Row { keys: vec![Some("shared".into())], clicks: 10.0, impressions: 100.0, ctr: 0.1, position: 5.0 },
            Row { keys: vec![Some("only1".into())], clicks: 70.0, impressions: 700.0, ctr: 0.1, position: 8.0 },
        ];
        let p2 = vec![Row {
            keys: vec![Some("shared".into())],
            clicks: 20.0,
            impressions: 150.0,
            ctr: 0.13,
            position: 4.0,
        }];
        // period2's fetch hit max_rows: "only1" missing from p2 is unknown, not zero
        let out = merge_compare(p1, p2, &dims, false, true);
        let by_q: HashMap<String, &JsonMap> = out
            .iter()
            .map(|m| (m.get("query").unwrap().as_str().unwrap().to_string(), m))
            .collect();
        let incomplete = by_q["only1"];
        assert_eq!(incomplete.get("data_incomplete").unwrap(), "period2");
        for field in ["period2_clicks", "period2_impressions", "period2_ctr", "period2_position",
                      "delta_clicks", "delta_impressions", "delta_ctr", "delta_position"] {
            assert!(incomplete.get(field).unwrap().is_null(), "{field} should be null");
        }
        // the fetched side keeps its real numbers
        assert_eq!(getf(incomplete, "period1_clicks"), 70.0);
        // both-sided rows are unaffected by the truncation flag
        assert_eq!(getf(by_q["shared"], "delta_clicks"), 10.0);
        assert!(!by_q["shared"].contains_key("data_incomplete"));
    }

    #[test]
    fn merge_compare_one_sided_rows_stay_zero_when_period_complete() {
        // Missing from a fully-fetched period => genuinely zero, no data_incomplete.
        let dims = vec!["query".to_string()];
        let p1 = vec![Row { keys: vec![Some("only1".into())], clicks: 4.0, impressions: 40.0, ctr: 0.1, position: 8.0 }];
        let out = merge_compare(p1, vec![], &dims, true, false);
        assert_eq!(getf(&out[0], "period2_clicks"), 0.0);
        assert_eq!(getf(&out[0], "delta_clicks"), -4.0);
        assert!(!out[0].contains_key("data_incomplete"));
    }

    #[test]
    fn drop_incomplete_rows_excludes_and_counts() {
        let mut complete = JsonMap::new();
        complete.insert("query".into(), Value::String("a".into()));
        let mut incomplete = JsonMap::new();
        incomplete.insert("query".into(), Value::String("b".into()));
        incomplete.insert("data_incomplete".into(), Value::String("period2".into()));
        let (kept, excluded) = drop_incomplete_rows(vec![complete, incomplete.clone(), incomplete]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].get("query").unwrap(), "a");
        assert_eq!(excluded, 2);
    }

    // --- regex escaping ---

    #[test]
    fn re2_escape_escapes_metacharacters() {
        assert_eq!(re2_escape("acme corp"), "acme corp");
        assert_eq!(re2_escape("a.b+c"), "a\\.b\\+c");
        assert_eq!(re2_escape("(x|y)"), "\\(x\\|y\\)");
    }
}
