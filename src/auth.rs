//! OAuth 2.0 Desktop-app flow with a Python-compatible `token.json`.
//!
//! The token file uses the same shape google-auth writes, so a token minted by the
//! Python server (or vice versa) is interchangeable. Running as an MCP stdio server
//! this module NEVER opens a browser — a missing/invalid token surfaces as a clear
//! error dict. Only the `--auth` CLI path is interactive.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{GscError, NOT_AUTHENTICATED_MSG};

pub const SCOPES: [&str; 2] = [
    "https://www.googleapis.com/auth/webmasters.readonly",
    "https://www.googleapis.com/auth/webmasters",
];

const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// Refresh this long before the recorded expiry to absorb clock skew.
const EXPIRY_SLACK_SECS: i64 = 60;

#[derive(Debug, Deserialize)]
struct ClientSecretsFile {
    installed: Option<ClientSecrets>,
    web: Option<ClientSecrets>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientSecrets {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_auth_uri")]
    pub auth_uri: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_auth_uri() -> String {
    "https://accounts.google.com/o/oauth2/auth".to_string()
}

fn default_token_uri() -> String {
    DEFAULT_TOKEN_URI.to_string()
}

/// google-auth's authorized-user token file format (what `Credentials.to_json()` writes).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub universe_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<String>,
    /// Any fields this port doesn't model (e.g. google-auth's rapt_token) are
    /// preserved verbatim so the file stays interchangeable with the Python server.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn parse_expiry(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.fZ", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc());
        }
    }
    None
}

fn format_expiry(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
}

struct CachedToken {
    access_token: String,
    expiry: DateTime<Utc>,
}

pub struct AuthManager {
    http: reqwest::Client,
    creds_path: PathBuf,
    token_path: PathBuf,
    cached: tokio::sync::Mutex<Option<CachedToken>>,
}

impl AuthManager {
    pub fn new(base_dir: &Path) -> Self {
        AuthManager {
            // The token cache mutex is held across refresh requests (single-flight),
            // so the OAuth client must have a hard timeout or one hung token-endpoint
            // connection would block every tool call indefinitely.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            creds_path: base_dir.join("gsccredentials.json"),
            token_path: base_dir.join("token.json"),
            cached: tokio::sync::Mutex::new(None),
        }
    }

    pub fn token_path(&self) -> &Path {
        &self.token_path
    }

    fn load_token_file(&self) -> Option<TokenFile> {
        let text = std::fs::read_to_string(&self.token_path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn load_client_secrets(&self) -> Result<ClientSecrets, GscError> {
        let text = std::fs::read_to_string(&self.creds_path).map_err(|_| {
            GscError::invalid(format!(
                "Missing OAuth client secrets file '{}' in {}. Obtain gsccredentials.json \
                 (shared privately, e.g. via 1Password) and place it next to the binary's \
                 project root, then re-run with --auth.",
                self.creds_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                self.creds_path.parent().unwrap_or(Path::new(".")).display(),
            ))
        })?;
        let parsed: ClientSecretsFile = serde_json::from_str(&text)
            .map_err(|e| GscError::invalid(format!("Malformed gsccredentials.json: {e}")))?;
        parsed
            .installed
            .or(parsed.web)
            .ok_or_else(|| GscError::invalid("gsccredentials.json has no 'installed' or 'web' section."))
    }

    fn save_token_file(&self, tf: &TokenFile) -> Result<(), GscError> {
        let body = serde_json::to_string(tf)
            .map_err(|e| GscError::invalid(format!("Could not serialize token: {e}")))?;
        // Write to a sibling temp file (private from creation, not chmod'd after the
        // fact) and rename over token.json — atomic on same filesystem, so a crash
        // mid-write can't truncate a valid refresh token, and no window exists where
        // the file is world-readable.
        let mut tmp_name = self.token_path.clone().into_os_string();
        tmp_name.push(".tmp");
        let tmp_path = PathBuf::from(tmp_name);
        let write = || -> std::io::Result<()> {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp_path)?;
            use std::io::Write;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &self.token_path)?;
            Ok(())
        };
        write().map_err(|e| {
            GscError::invalid(format!("Could not write {}: {e}", self.token_path.display()))
        })
    }

    /// Returns a valid access token, refreshing (and persisting) if needed.
    /// Never interactive: a missing token is an actionable error, not a hung browser.
    pub async fn get_access_token(&self) -> Result<String, GscError> {
        let mut cached = self.cached.lock().await;
        let now = Utc::now();
        if let Some(c) = cached.as_ref() {
            if c.expiry - now > Duration::seconds(EXPIRY_SLACK_SECS) {
                return Ok(c.access_token.clone());
            }
        }

        let Some(tf) = self.load_token_file() else {
            return Err(GscError::invalid(NOT_AUTHENTICATED_MSG));
        };

        // Still-valid stored token: use it without a refresh round-trip.
        if let (Some(token), Some(expiry)) = (
            tf.token.clone(),
            tf.expiry.as_deref().and_then(parse_expiry),
        ) {
            if expiry - now > Duration::seconds(EXPIRY_SLACK_SECS) {
                *cached = Some(CachedToken { access_token: token.clone(), expiry });
                return Ok(token);
            }
        }

        let Some(refresh_token) = tf.refresh_token.clone() else {
            return Err(GscError::invalid(NOT_AUTHENTICATED_MSG));
        };

        match self.refresh(&tf, &refresh_token).await {
            Ok(parsed) => {
                let expiry = Utc::now() + Duration::seconds(parsed.expires_in.unwrap_or(3600));
                let access_token = parsed.access_token;
                let mut updated = tf;
                updated.token = Some(access_token.clone());
                updated.expiry = Some(format_expiry(expiry));
                // Google may rotate the refresh token; keep the newest one.
                if let Some(rt) = parsed.refresh_token {
                    updated.refresh_token = Some(rt);
                }
                self.save_token_file(&updated)?;
                *cached = Some(CachedToken { access_token: access_token.clone(), expiry });
                Ok(access_token)
            }
            Err(RefreshFailure::Rejected(detail)) => {
                // Token revoked/expired beyond refresh — drop it so the next attempt re-auths
                // cleanly (mirrors the Python server deleting token.json on RefreshError).
                tracing::warn!("Token refresh rejected; removing stale token: {detail}");
                let _ = std::fs::remove_file(&self.token_path);
                *cached = None;
                Err(GscError::Auth(detail))
            }
            Err(RefreshFailure::Transient(detail)) => Err(GscError::Api { status: None, raw: detail }),
        }
    }

    async fn refresh(
        &self,
        tf: &TokenFile,
        refresh_token: &str,
    ) -> Result<TokenResponse, RefreshFailure> {
        // Prefer identifiers stored in the token file (google-auth writes them);
        // fall back to the client secrets file.
        let (client_id, client_secret) = match (tf.client_id.clone(), tf.client_secret.clone()) {
            (Some(id), Some(secret)) => (id, secret),
            _ => {
                let secrets = self
                    .load_client_secrets()
                    .map_err(|e| RefreshFailure::Rejected(e.message()))?;
                (secrets.client_id, secrets.client_secret)
            }
        };
        let token_uri = tf.token_uri.clone().unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string());
        let params = [
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ];
        let resp = self
            .http
            .post(&token_uri)
            .form(&params)
            .send()
            .await
            .map_err(|e| RefreshFailure::Transient(format!("token refresh request failed: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = format!("HTTP {}: {}", status.as_u16(), text.trim());
            // Only a definitive grant rejection may destroy the stored token; a
            // transient 408/429/5xx must not force interactive re-authentication.
            let oauth_error = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str().map(String::from)));
            let rejected = match oauth_error.as_deref() {
                Some(
                    "invalid_grant" | "invalid_client" | "unauthorized_client" | "invalid_scope"
                    | "invalid_rapt",
                ) => true,
                Some(_) => false,
                None => matches!(status.as_u16(), 400 | 401 | 403),
            };
            return Err(if rejected {
                RefreshFailure::Rejected(detail)
            } else {
                RefreshFailure::Transient(detail)
            });
        }
        serde_json::from_str(&text)
            .map_err(|e| RefreshFailure::Transient(format!("bad token response: {e}")))
    }

    /// `--auth` CLI path: interactive browser sign-in (no-op if the stored token is usable).
    /// Returns true if a valid token already existed and nothing was changed.
    pub async fn interactive_auth(&self) -> Result<bool, GscError> {
        // A still-valid stored token is a clean no-op (report "no changes made").
        // An expired-but-refreshable one is silently refreshed and reported as a
        // successful (re)authentication, mirroring the Python pre-refresh validity check.
        let preexisting_valid = self
            .load_token_file()
            .map(|tf| {
                tf.token.is_some()
                    && tf
                        .expiry
                        .as_deref()
                        .and_then(parse_expiry)
                        .is_some_and(|exp| exp - Utc::now() > Duration::seconds(EXPIRY_SLACK_SECS))
            })
            .unwrap_or(false);
        if preexisting_valid {
            return Ok(true);
        }
        if self.load_token_file().is_some() && self.get_access_token().await.is_ok() {
            return Ok(false);
        }

        let secrets = self.load_client_secrets()?;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| GscError::invalid(format!("Could not open local OAuth listener: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| GscError::invalid(format!("Could not read listener address: {e}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}");

        // CSRF state and PKCE verifier both gate the loopback flow, so they must be
        // unpredictable — OS randomness, not a userspace PRNG.
        let state = hex_string(&random_bytes(16)?);
        // RFC 7636 S256: verifier is 43 base64url chars from 32 random bytes,
        // challenge is base64url(SHA-256(verifier)).
        let code_verifier = base64url(&random_bytes(32)?);
        let code_challenge = base64url(&sha2::Sha256::digest(code_verifier.as_bytes()));
        let scope = SCOPES.join(" ");
        let auth_url = format!(
            "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline\
             &prompt=consent&state={}&code_challenge={}&code_challenge_method=S256",
            secrets.auth_uri,
            urlencode(&secrets.client_id),
            urlencode(&redirect_uri),
            urlencode(&scope),
            state,
            code_challenge,
        );

        eprintln!("Opening browser for Google sign-in...");
        eprintln!("If nothing opens, visit:\n  {auth_url}");
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(&auth_url).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let _ = std::process::Command::new("xdg-open").arg(&auth_url).spawn();
        // rundll32 takes the URL as a plain argument — unlike `cmd /C start`, no shell
        // parsing of the `&`s in the query string.
        #[cfg(windows)]
        let _ = std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", &auth_url])
            .spawn();

        // Bound the overall wait — if the user closes the browser without completing
        // sign-in, don't hang the --auth invocation forever.
        let code = match tokio::time::timeout(
            std::time::Duration::from_secs(300),
            wait_for_code(listener, &state),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(GscError::invalid(
                    "Timed out waiting for the OAuth redirect after 5 minutes. Re-run with \
                     --auth and complete the sign-in in the browser."
                        .to_string(),
                ));
            }
        };

        let params = [
            ("code", code.as_str()),
            ("client_id", secrets.client_id.as_str()),
            ("client_secret", secrets.client_secret.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", code_verifier.as_str()),
        ];
        let resp = self
            .http
            .post(&secrets.token_uri)
            .form(&params)
            .send()
            .await
            .map_err(|e| GscError::invalid(format!("Token exchange failed: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(GscError::invalid(format!(
                "Token exchange failed (HTTP {}): {}",
                status.as_u16(),
                text.trim()
            )));
        }
        let parsed: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| GscError::invalid(format!("Bad token response: {e}")))?;
        let expiry = Utc::now() + Duration::seconds(parsed.expires_in.unwrap_or(3600));

        let tf = TokenFile {
            token: Some(parsed.access_token.clone()),
            refresh_token: parsed.refresh_token,
            token_uri: Some(secrets.token_uri.clone()),
            client_id: Some(secrets.client_id.clone()),
            client_secret: Some(secrets.client_secret.clone()),
            scopes: Some(SCOPES.iter().map(|s| s.to_string()).collect()),
            universe_domain: Some("googleapis.com".to_string()),
            account: Some(String::new()),
            expiry: Some(format_expiry(expiry)),
            extra: serde_json::Map::new(),
        };
        self.save_token_file(&tf)?;
        let mut cached = self.cached.lock().await;
        *cached = Some(CachedToken { access_token: parsed.access_token, expiry });
        Ok(false)
    }
}

enum RefreshFailure {
    /// The authorization server rejected the refresh (revoked/expired grant).
    Rejected(String),
    /// Network/server trouble — the token may still be fine.
    Transient(String),
}

fn urlencode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Cryptographically secure random bytes from the OS.
fn random_bytes(n: usize) -> Result<Vec<u8>, GscError> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf)
        .map_err(|e| GscError::invalid(format!("OS randomness unavailable: {e}")))?;
    Ok(buf)
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Unpadded base64url (RFC 4648 §5) — the PKCE alphabet.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..(chunk.len() + 1) {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// Read an HTTP request head (through the blank line) with a size cap.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> std::io::Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65536 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

async fn respond(stream: &mut tokio::net::TcpStream, status_line: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

/// Accept loopback connections until the OAuth redirect arrives; return the auth code.
///
/// Robustness rules: each connection gets a bounded read (10s, 64KB) so a stalled
/// client can't block sign-in; the `state` parameter is validated BEFORE `error` or
/// `code` are honored, so a stray or forged request can neither abort nor hijack a
/// legitimate sign-in — it is answered and the listener keeps waiting.
async fn wait_for_code(listener: tokio::net::TcpListener, expected_state: &str) -> Result<String, GscError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| GscError::invalid(format!("OAuth listener failed: {e}")))?;
        let request = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_request_head(&mut stream),
        )
        .await
        {
            Ok(Ok(r)) => r,
            _ => continue, // slow or broken connection — keep waiting for the real redirect
        };
        let first_line = request.lines().next().unwrap_or_default();
        // Expect: GET /?state=...&code=... HTTP/1.1
        let path = first_line.split_whitespace().nth(1).unwrap_or_default();
        let query = path.split_once('?').map(|x| x.1).unwrap_or_default();
        let mut code = None;
        let mut state = None;
        let mut oauth_error = None;
        for pair in query.split('&') {
            let mut kv = pair.splitn(2, '=');
            let k = kv.next().unwrap_or_default();
            let v = kv.next().unwrap_or_default();
            let v = percent_encoding::percent_decode_str(v)
                .decode_utf8_lossy()
                .to_string();
            match k {
                "code" => code = Some(v),
                "state" => state = Some(v),
                "error" => oauth_error = Some(v),
                _ => {}
            }
        }
        if state.as_deref() != Some(expected_state) {
            // Not our redirect (favicon probe, stray request, or forged state).
            respond(&mut stream, "404 Not Found", "").await;
            continue;
        }
        if let Some(err) = oauth_error {
            respond(
                &mut stream,
                "200 OK",
                "<html><body><p>Authorization was denied &mdash; you may close this window.</p></body></html>",
            )
            .await;
            return Err(GscError::invalid(format!("Authorization was denied: {err}")));
        }
        if let Some(code) = code {
            respond(
                &mut stream,
                "200 OK",
                "<html><body><p>Authentication complete &mdash; you may close this window.</p></body></html>",
            )
            .await;
            return Ok(code);
        }
        respond(&mut stream, "404 Not Found", "").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc4648() {
        assert_eq!(base64url(b"abc"), "YWJj");
        assert_eq!(base64url(&[0xff]), "_w");
        assert_eq!(base64url(&[0xfb, 0xef]), "--8");
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64url(&sha2::Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_is_43_chars_of_pkce_alphabet() {
        let v = base64url(&[0u8; 32]);
        assert_eq!(v.len(), 43);
    }
}
