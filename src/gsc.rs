//! Thin REST client for the two Google Search Console APIs
//! (webmasters v3 and searchconsole v1 URL Inspection), with the same
//! retry-on-transient-failure behavior as googleapiclient's num_retries.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use serde_json::Value;

use crate::auth::AuthManager;
use crate::error::GscError;

pub const WEBMASTERS_BASE: &str = "https://www.googleapis.com/webmasters/v3";
pub const INSPECT_ENDPOINT: &str = "https://searchconsole.googleapis.com/v1/urlInspection/index:inspect";
/// Retries after the first attempt on 5xx/network errors (matches API_NUM_RETRIES).
pub const API_NUM_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct GscClient {
    http: reqwest::Client,
    auth: Arc<AuthManager>,
}

/// Percent-encode a URL path segment (site URLs contain `/` and `:`).
pub fn encode_segment(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn extract_google_error(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    v.get("error")?.get("message")?.as_str().map(|s| s.to_string())
}

impl GscClient {
    pub fn new(auth: Arc<AuthManager>) -> Self {
        GscClient {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            auth,
        }
    }

    async fn request_json(
        &self,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value, GscError> {
        let mut last_err = GscError::Api { status: None, raw: "request not attempted".into() };
        for attempt in 0..=API_NUM_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(1u64 << (attempt - 1))).await;
            }
            let token = self.auth.get_access_token().await?;
            let mut req = self.http.request(method.clone(), url).bearer_auth(&token);
            if let Some(b) = body {
                req = req.json(b);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    // A network failure while reading the body is retryable — it must NOT
                    // masquerade as an empty/zero-row success (which would then be cached).
                    let text = match resp.text().await {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!("GSC API body read failed on {url} (attempt {attempt}): {e}");
                            last_err = GscError::Api {
                                status: None,
                                raw: format!("failed reading API response: {e}"),
                            };
                            continue;
                        }
                    };
                    if status.is_success() {
                        if text.trim().is_empty() {
                            return Ok(Value::Null);
                        }
                        return serde_json::from_str(&text).map_err(|e| GscError::Api {
                            status: None,
                            raw: format!("invalid JSON in API response: {e}"),
                        });
                    }
                    let code = status.as_u16();
                    let msg = extract_google_error(&text).unwrap_or_else(|| text.trim().to_string());
                    let err = GscError::Api { status: Some(code), raw: format!("HTTP {code}: {msg}") };
                    if status.is_server_error() {
                        tracing::warn!("GSC API {code} on {url} (attempt {attempt}), retrying");
                        last_err = err;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) => {
                    tracing::warn!("GSC API request error on {url} (attempt {attempt}): {e}");
                    last_err = GscError::Api { status: None, raw: format!("request failed: {e}") };
                    continue;
                }
            }
        }
        Err(last_err)
    }

    /// `sites.list` — returns the `siteEntry` array (possibly empty).
    pub async fn list_sites(&self) -> Result<Value, GscError> {
        let v = self
            .request_json(Method::GET, &format!("{WEBMASTERS_BASE}/sites"), None)
            .await?;
        Ok(v.get("siteEntry").cloned().unwrap_or_else(|| Value::Array(vec![])))
    }

    /// `searchanalytics.query` — returns the raw response object.
    pub async fn query_search_analytics(&self, site_url: &str, body: &Value) -> Result<Value, GscError> {
        let url = format!(
            "{WEBMASTERS_BASE}/sites/{}/searchAnalytics/query",
            encode_segment(site_url)
        );
        self.request_json(Method::POST, &url, Some(body)).await
    }

    /// `sitemaps.list` — returns the `sitemap` array (possibly empty).
    pub async fn list_sitemaps(&self, site_url: &str) -> Result<Value, GscError> {
        let url = format!("{WEBMASTERS_BASE}/sites/{}/sitemaps", encode_segment(site_url));
        let v = self.request_json(Method::GET, &url, None).await?;
        Ok(v.get("sitemap").cloned().unwrap_or_else(|| Value::Array(vec![])))
    }

    pub async fn submit_sitemap(&self, site_url: &str, feedpath: &str) -> Result<(), GscError> {
        let url = format!(
            "{WEBMASTERS_BASE}/sites/{}/sitemaps/{}",
            encode_segment(site_url),
            encode_segment(feedpath)
        );
        self.request_json(Method::PUT, &url, None).await.map(|_| ())
    }

    pub async fn delete_sitemap(&self, site_url: &str, feedpath: &str) -> Result<(), GscError> {
        let url = format!(
            "{WEBMASTERS_BASE}/sites/{}/sitemaps/{}",
            encode_segment(site_url),
            encode_segment(feedpath)
        );
        self.request_json(Method::DELETE, &url, None).await.map(|_| ())
    }

    /// `urlInspection.index.inspect` — returns `inspectionResult` (or the raw response).
    pub async fn inspect_url(
        &self,
        inspection_url: &str,
        site_url: &str,
        language_code: &str,
    ) -> Result<Value, GscError> {
        let body = serde_json::json!({
            "inspectionUrl": inspection_url,
            "siteUrl": site_url,
            "languageCode": language_code,
        });
        let v = self.request_json(Method::POST, INSPECT_ENDPOINT, Some(&body)).await?;
        Ok(v.get("inspectionResult").cloned().unwrap_or(v))
    }
}
