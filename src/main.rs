//! Rust MCP server for Google Search Console (stdio transport).
//!
//! Run with no arguments to serve MCP over stdio; run `--auth` for the
//! interactive browser sign-in (mirrors `python server.py --auth`).

mod analytics;
mod auth;
mod error;
mod gsc;
mod prompts;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ServiceExt;

use crate::analytics::AppState;
use crate::auth::AuthManager;
use crate::gsc::GscClient;
use crate::server::GscServer;

/// Credentials directory: $GSC_MCP_DIR if set, else the crate directory
/// (mirrors the Python server keeping gsccredentials.json/token.json next to server.py).
fn base_dir() -> PathBuf {
    match std::env::var("GSC_MCP_DIR") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")),
    }
}

async fn run_auth(auth: Arc<AuthManager>) -> i32 {
    let preexisting_valid = match auth.interactive_auth().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Authentication failed: {}", e.message());
            return 1;
        }
    };
    if preexisting_valid {
        println!(
            "Already authenticated — existing token at {} is valid; no changes made.",
            auth.token_path().display()
        );
    } else {
        println!("Authentication successful — token saved to {}.", auth.token_path().display());
    }

    // Best-effort liveness check: confirm the credential works and show which
    // properties this Google account can see. Informational only.
    let client = GscClient::new(auth);
    match client.list_sites().await {
        Ok(sites) => {
            let entries = sites.as_array().cloned().unwrap_or_default();
            let noun = if entries.len() == 1 { "property" } else { "properties" };
            println!(
                "Signed-in Google account has access to {} Search Console {noun}.",
                entries.len()
            );
            for s in entries.iter().take(10) {
                println!(
                    "  - {} ({})",
                    s.get("siteUrl").and_then(|v| v.as_str()).unwrap_or("?"),
                    s.get("permissionLevel").and_then(|v| v.as_str()).unwrap_or("?"),
                );
            }
            if entries.len() > 10 {
                println!("  ... and {} more", entries.len() - 10);
            }
        }
        Err(e) => println!("(Could not list properties: {})", e.message()),
    }
    0
}

#[tokio::main]
async fn main() {
    // Logs go to stderr — stdout carries the MCP protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let auth = Arc::new(AuthManager::new(&base_dir()));

    if std::env::args().any(|a| a == "--auth") {
        std::process::exit(run_auth(auth).await);
    }

    let client = GscClient::new(auth);
    let state = Arc::new(AppState::new(client));
    let server = GscServer::new(state);

    let service = match server.serve(rmcp::transport::stdio()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start MCP server: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = service.waiting().await {
        eprintln!("Server terminated with error: {e}");
        std::process::exit(1);
    }
}
