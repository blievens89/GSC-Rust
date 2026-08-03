//! Error type shared by all tools. Mirrors the Python `gsc_tool` decorator contract:
//! tools never raise to the MCP client — they return a classified error dict.

use serde_json::{json, Value};

pub const NOT_AUTHENTICATED_MSG: &str =
    "Not authenticated. Run 'gsc-mcp-rs --auth' (or: cargo run -- --auth) to sign in with your Google account.";

pub const REAUTH_MSG: &str = "Re-authentication required — the OAuth token is invalid or revoked. \
     Run 'gsc-mcp-rs --auth' (or: cargo run -- --auth) to re-authenticate.";

#[derive(Debug, Clone)]
pub enum GscError {
    /// Bad tool input (Python: ValueError -> {"error": "invalid_input"}).
    InvalidInput(String),
    /// Google API failure (Python: HttpError -> {"error": "api_error"}).
    Api { status: Option<u16>, raw: String },
    /// OAuth token invalid/revoked (Python: RefreshError -> {"error": "auth_error"}).
    Auth(String),
}

impl GscError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        GscError::InvalidInput(msg.into())
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            GscError::Api { status, .. } => *status,
            _ => None,
        }
    }

    /// A short human-readable message (used where Python interpolates `str(e)`).
    pub fn message(&self) -> String {
        match self {
            GscError::InvalidInput(m) => m.clone(),
            GscError::Api { raw, .. } => raw.clone(),
            GscError::Auth(m) => m.clone(),
        }
    }

    /// The structured error dict returned to the MCP client in place of a result.
    pub fn to_error_value(&self) -> Value {
        match self {
            GscError::InvalidInput(m) => json!({"error": "invalid_input", "message": m}),
            GscError::Api { status, raw } => json!({
                "error": "api_error",
                "status": status,
                "message": classify_http_error(*status, raw),
            }),
            GscError::Auth(_) => json!({"error": "auth_error", "message": REAUTH_MSG}),
        }
    }
}

impl std::fmt::Display for GscError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for GscError {}

pub fn classify_http_error(status: Option<u16>, raw: &str) -> String {
    let hint = match status {
        Some(400) => {
            "Bad request — check date formats (YYYY-MM-DD) and the site_url \
             (use 'https://example.com/' with a trailing slash, or 'sc-domain:example.com')."
        }
        Some(403) => {
            "Permission denied — the connected Google account may not have access to \
             this property, or the wrong account is authenticated."
        }
        Some(404) => "Not found — check the site_url format and that the property exists in this account.",
        Some(429) => {
            "Quota exceeded — too many requests. Wait and retry \
             (the URL Inspection API allows ~2000/day)."
        }
        _ => "Google API error.",
    };
    format!("{hint} ({raw})")
}
