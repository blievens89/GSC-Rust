//! The 7 guided-workflow prompt texts, ported verbatim from the Python server
//! (the Python source's accidental doubled %% signs are rendered as single %).

pub fn seo_audit(site_url: &str, brand_terms: Option<&[String]>, days: u32) -> String {
    let brand_step = match brand_terms {
        Some(terms) if !terms.is_empty() => {
            let terms_str = terms.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(", ");
            format!(
                "\n8. **Branded vs Non-Branded**: Run `branded_performance` with brand_terms=[{terms_str}] to understand the brand/non-brand traffic split."
            )
        }
        _ => String::new(),
    };
    format!(
        r#"Perform a comprehensive SEO audit for {site_url}. Follow these steps in order:

**Phase 1 — Baseline & Trend**
1. **Performance Summary**: Run `performance_summary` with days={days} to get baseline metrics and period-over-period deltas.
2. **Date Trend**: Run `date_trend` to see the daily traffic trend — identify any sudden drops, spikes, or inflection points.
3. **Position Distribution**: Run `position_distribution` to understand the ranking profile.

**Phase 2 — Asset Inventory**
4. **Top Queries**: Run `top_queries` with limit=15 to identify the site's most valuable search terms.
5. **Top Pages**: Run `top_pages` with limit=15 to identify the highest-traffic landing pages.
6. **Search Appearance**: Run `search_appearance` to see which SERP features the site appears in.
7. **Sitemaps**: Run `list_sitemaps` for {site_url} to check sitemap health (errors, warnings, last download dates).
{brand_step}
**Phase 3 — Diagnostics**
9. **Device Breakdown**: Run `device_breakdown` to check for mobile vs desktop performance gaps (important for mobile-first indexing).
10. **Quick Wins**: Run `quick_wins` to find low-hanging optimization opportunities (positions 4-20, high impressions).
11. **Content Decay**: Run `content_decay` to find pages losing traffic that need content refresh.
12. **Cannibalization**: Run `cannibalization` to find keyword conflicts between pages.
13. **Batch Inspect**: Run `batch_inspect` on the top 5 pages from step 5 to check indexing status, mobile usability, and crawl health.

After gathering all data, produce a structured report:

**Executive Summary** (3-4 sentences): Overall health, trend direction, biggest risk, biggest opportunity.

**Key Metrics Dashboard**: Current period clicks, impressions, CTR, avg position — each with period-over-period delta and percentage change.

**Top 5 Priority Actions** (ordered by estimated traffic impact):
For each action, specify:
- The specific page(s) and/or query/queries affected
- What to do (be concrete — not "optimize content" but "add section covering [subtopic] to [page URL]")
- Estimated impact (based on impression volume and position gap)
- Effort level (quick fix / moderate / major)

**Detailed Findings** by category: Quick wins, decaying content, cannibalization issues, device gaps, SERP feature opportunities.

**Health Flags**: Any sitemap errors, indexing issues from batch inspect, missing SERP features the site should target, or significant mobile vs desktop gaps."#
    )
}

pub fn content_opportunities(site_url: &str, days: u32) -> String {
    format!(
        r#"Find content optimization opportunities for {site_url}. Run these tools:

**Step 1 — Opportunity Landscape**
1. **Position Distribution**: Run `position_distribution` to understand how many queries are in striking distance vs. out of reach.
2. **Quick Wins**: Run `quick_wins` for {site_url} — queries in positions 4-20 with high impressions. These are the highest-ROI optimizations.

**Step 2 — Growth Signals**
3. **Trending Queries**: Run `trending_queries` with days={days} to find rising search terms you should capitalize on.
4. **New Queries**: Run `new_lost_queries` with days={days} to find newly appearing queries — potential new content topics.
5. **Top Growing Pages**: Run `top_growing_pages` with days={days} to see which pages are gaining momentum.

**Step 3 — Deep Dive on Top Opportunities**
6. **Cannibalization Check**: Run `cannibalization` to verify top opportunity queries aren't being competed for by multiple pages.
7. **Search Appearance**: Run `search_appearance` to identify SERP feature opportunities (FAQ, HowTo, etc.) you're not yet capturing.
8. For the top 3 quick-win pages, run `landing_page_queries` on each to understand what other queries those pages attract. This reveals content gaps and internal linking opportunities.

Synthesize into a prioritized opportunity list:

**High-Impact Quick Wins** (position 4-10, high impressions):
- For each: query, current page, current position, impressions, estimated click gain if moved to position 3
- Specific optimization action: title tag changes, content additions, internal links to add

**Content Gap Opportunities** (from new/trending queries with no dedicated page):
- Query cluster, combined impression volume, recommended content format (blog post, landing page, FAQ section)

**Growing Content to Double Down On**:
- Pages gaining traffic — what's working and how to accelerate it (expand content, build links, add schema markup)

**Cannibalization to Resolve**:
- Queries with multiple competing pages — recommend which page to consolidate to and what to do with the others (301 redirect, differentiate intent, noindex)

**SERP Feature Opportunities**:
- Rich result types the site doesn't appear in but could target with structured data"#
    )
}

pub fn traffic_diagnosis(site_url: &str, brand_terms: Option<&[String]>) -> String {
    let brand_step = match brand_terms {
        Some(terms) if !terms.is_empty() => {
            let terms_str = terms.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(", ");
            format!(
                "\n5. **Branded vs Non-Branded**: Run `branded_performance` with brand_terms=[{terms_str}] to determine if the drop affects brand queries, non-brand queries, or both. This distinguishes SEO issues from brand awareness changes."
            )
        }
        _ => String::new(),
    };
    format!(
        r#"Diagnose potential traffic issues for {site_url}. Follow this workflow:

**Phase 1 — Establish the Trend**
1. **Performance Summary**: Run `performance_summary` with days=28 to see current vs previous period.
2. **Date Trend (wide)**: Run `date_trend` with a 90-day window (calculate start_date as 93 days ago, end_date as 3 days ago) to see the full traffic trajectory and pinpoint when any decline began.
3. **Date Trend (recent)**: Run `date_trend` with the default 28-day window for a detailed recent view. Look for sudden drops vs gradual declines.

**Phase 2 — Scope the Problem**
4. **Content Decay**: Run `content_decay` to identify which pages lost the most traffic. Note whether losses are concentrated on a few pages or spread broadly.
{brand_step}
6. **Lost Queries**: Run `new_lost_queries` to find queries that completely disappeared.
7. **Device Breakdown**: Run `device_breakdown` to check if losses are device-specific (a mobile-only drop suggests Core Web Vitals or mobile usability issues).
8. **Country Breakdown**: Run `country_breakdown` to check if losses are geography-specific.
9. **Search Appearance**: Run `search_appearance` to check if the site lost any SERP features (rich results, FAQ, sitelinks).

**Phase 3 — Technical Health Check**
10. **Sitemaps**: Run `list_sitemaps` to check for sitemap errors or warnings.
11. **URL Inspection**: For the top 3-5 pages from content_decay results, run `batch_inspect` to check indexing status, mobile usability, and crawl issues.

Analyze and provide:

**Timeline**: When exactly did the decline start? Is it sudden (likely algorithm/technical) or gradual (likely content decay/competition)?

**Scope**: Is it site-wide, page-specific, or query-specific? Quantify: what percentage of total traffic is affected?

**Root Cause Analysis** (rank by likelihood):
- Algorithm update (correlate the drop date with known Google updates)
- Technical issues (indexing errors, mobile usability problems, sitemap errors found in inspection)
- Content decay (aging content losing relevance)
- Lost SERP features (previously had rich results, now don't)
- Competitive displacement (position drops without technical issues)
- Seasonal patterns (compare with the nature of top-affected queries)

**Remediation Plan** (ordered by expected impact):
For each action: what to do, which pages/queries it addresses, expected recovery timeline, and effort level."#
    )
}

pub fn page_deep_dive(site_url: &str, page_url: &str) -> String {
    format!(
        r#"Perform a deep-dive SEO analysis of this page:
- Site: {site_url}
- Page: {page_url}

Run these tools in order:

**Phase 1 — Technical Health**
1. **URL Inspection**: Run `inspect_url` for {page_url} to check indexing status, mobile usability, rich results eligibility, and last crawl date.

**Phase 2 — Performance Data**
2. **Landing Page Queries**: Run `landing_page_queries` for {page_url} with limit=50 to see all queries driving traffic to this page.
3. **Device Breakdown**: Run `device_breakdown` with page_filter="{page_url}" to see desktop vs mobile vs tablet performance.
4. **Period Comparison**: Run `compare_periods` with dimensions=["query"], and a page filter (dimension_filter_groups=[{{"filters": [{{"dimension": "page", "operator": "equals", "expression": "{page_url}"}}]}}]) comparing the last 28 days vs the previous 28 days. This shows whether the page is growing or declining for each query.

**Phase 3 — Competitive Context**
5. **Cannibalization Check**: Run `cannibalization` for {site_url}. From the results, check if any of this page's top 5 queries appear — meaning other pages on the site compete for the same terms.

Provide a structured analysis:

**Technical Status**: Indexing state, last crawl date, mobile usability pass/fail, rich results eligibility. Flag any issues.

**Traffic Profile**:
- Total clicks and impressions (current period)
- Period-over-period trend: growing, stable, or declining? By how much?
- Device split: is traffic heavily skewed? Is mobile CTR significantly lower than desktop (suggesting mobile UX issues)?

**Query Analysis**:
- Top 10 queries by clicks, with position and CTR for each
- For each query in positions 4-10: flag as a quick win opportunity
- For each query where CTR is below expected for its position range (benchmarks: pos 1 ~28%, pos 2 ~15%, pos 3 ~11%, pos 4-5 ~6-8%, pos 6-10 ~2-4%), flag as title/description optimization opportunity

**Cannibalization Issues**: If any top queries have multiple pages ranking, identify which page should be the canonical target and recommend consolidation actions.

**Optimization Recommendations**:
- Suggested title tag (incorporating top queries naturally, under 60 chars)
- Suggested meta description (incorporating secondary queries, compelling CTA, under 155 chars)
- Content gaps: queries where the page ranks but the topic isn't well-covered
- Internal linking opportunities: queries where other pages rank better — link from those pages to this one"#
    )
}

pub fn weekly_report(site_url: &str) -> String {
    format!(
        r#"Generate a weekly SEO performance report for {site_url}.

**Data Collection** — run all tools with 7-day periods:

1. **Performance Summary**: Run `performance_summary` with days=7 for headline metrics with week-over-week deltas.
2. **Date Trend**: Run `date_trend` with start_date covering the last 14 days to show this week vs last week as a daily trend.
3. **Top Queries**: Run `top_queries` with limit=10 for current top performers.
4. **Trending Queries**: Run `trending_queries` with days=7 to identify queries gaining or losing momentum.
5. **Top Growing Pages**: Run `top_growing_pages` with days=7 to find pages on the rise.
6. **Content Decay**: Run `content_decay` with days=7 and min_previous_clicks=3 to catch early declines (lower threshold for weekly cadence).
7. **New/Lost Queries**: Run `new_lost_queries` with days=7 and min_impressions=5 to catch newly appearing or disappearing queries.
8. **Device Breakdown**: Run `device_breakdown` to check for device-specific shifts.
9. **Search Appearance**: Run `search_appearance` to flag any SERP feature changes.

**Report Format**:

# Weekly SEO Report — {site_url}
**Period**: [dates]

## Headline Metrics
| Metric | This Week | Last Week | Change | % Change |
Clicks, impressions, CTR, avg position — flag any metric that changed by more than 10%.

## Traffic Trend
Describe the daily pattern. Note any anomalies (weekend dips are normal, mid-week drops are not).

## Winners This Week
- Top 5 queries that gained the most clicks (with delta)
- Top 5 pages that gained the most clicks (with delta)

## Losers This Week
- Top 5 queries that lost the most clicks
- Top 5 pages that lost the most clicks — for any page losing >20% clicks, flag for investigation

## New Opportunities
- Newly appearing queries worth targeting (with impression volume)
- Any new SERP features the site appeared in

## Device & Technical Notes
- Any significant device split changes (e.g., mobile share dropped)

## Action Items for Next Week
3-5 specific, concrete tasks — each referencing specific queries or pages from the data above. Prioritize by estimated traffic impact."#
    )
}

pub fn international_audit(site_url: &str, top_n_countries: u32) -> String {
    format!(
        r#"Analyze international SEO performance for {site_url}.

1. **Country Breakdown**: Run `country_breakdown` with limit={top_n_countries} to identify top traffic countries.
2. **Performance Summary**: Run `performance_summary` for overall baseline.
3. For the top 3 countries by clicks, run `search_analytics` with dimensions=["query"] and a country filter (dimension_filter_groups=[{{"filters": [{{"dimension": "country", "operator": "equals", "expression": "<COUNTRY_CODE>"}}]}}]) with limit=15 to see what queries drive traffic in each country.
4. **Device Breakdown**: Run `device_breakdown` to check if device preferences differ across the site (some markets are heavily mobile-dominant).

Provide:
- Country-by-country performance table: clicks, impressions, CTR, avg position
- For top 3 countries: top 10 queries and whether they differ from the global top queries
- Identify countries where CTR is unusually low for their impression volume (potential localization opportunity)
- Identify countries with high impressions but low clicks (content not matching local search intent)
- Recommendations for geo-specific content, hreflang implementation, or market prioritization"#
    )
}

pub fn technical_health(site_url: &str) -> String {
    format!(
        r#"Run a technical SEO health check for {site_url}.

1. **Sitemaps**: Run `list_sitemaps` for {site_url}. Check for errors, warnings, and last download dates.
2. **Top Pages**: Run `top_pages` with limit=20 to get the site's most important pages by traffic.
3. **Batch Inspect**: Run `batch_inspect` on the top 10 pages from step 2 to check indexing status, mobile usability, crawl info, and rich results.
4. **Device Breakdown**: Run `device_breakdown` to compare mobile vs desktop performance. A large gap suggests mobile usability issues.
5. **Search Appearance**: Run `search_appearance` to see which structured data types are active.

Report:

**Sitemap Health**: Number of sitemaps, any errors/warnings, staleness (last download > 7 days ago = flag).

**Indexing Status**: For each inspected URL, report coverage state (Valid, Excluded, Error), robots.txt status, last crawl time, crawl agent (mobile/desktop).

**Mobile Readiness**: Mobile usability pass/fail for each page, mobile vs desktop CTR comparison (>50% gap = red flag).

**Rich Results**: Which schema types are deployed, which are eligible but not implemented.

**Priority Fixes** (ordered by severity — critical/warning/info):
Each fix should specify the affected URL(s), what's wrong, and the specific remediation step."#
    )
}
