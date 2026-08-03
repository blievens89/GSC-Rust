# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

MCP stdio server exposing Google Search Console data: 30 tools, 3 resources, 7 prompts,
OAuth 2.0 Desktop-app flow. Built on rmcp 2.2. Originated as a Rust port of a Python GSC
MCP server, which explains the Python-compatibility notes in Gotchas below.

29 tools mirror the original Python server 1:1. The 30th, `export_search_analytics`, is
Rust-only: pulls all rows the API exposes via `fetch_all_rows_concurrent` (pages fetched 4 at a time
with fixed startRows, reassembled in order, same exact-cap probe as `fetch_all_rows`),
written as CSV to disk with only a path + summary returned to the chat — the escape
hatch for bulk pulls the response size cap would truncate. Writes are confined to the
export root (`$GSC_EXPORT_DIR`, else `~/Downloads`): `output_path` is model-supplied
and untrusted, so absolute paths / `..` / `:` (Windows alternate data streams) /
reserved device names are rejected lexically, containment is re-verified against
canonicalized paths at write time (symlinks and junctions), and existing files are
never overwritten. Keep that confinement.

## Commands

```bash
cargo build --release          # optimized binary at target/release/gsc-mcp-rs
cargo test                     # unit tests for the analytics helpers
cargo clippy                   # lint
./target/release/gsc-mcp-rs --auth   # interactive browser sign-in (no-op if token valid)
./target/release/gsc-mcp-rs          # run MCP stdio server
```

Rust was installed via rustup with `--no-modify-path`; use `~/.cargo/bin/cargo` if `cargo`
is not on PATH.

## Architecture

- `src/main.rs` — CLI dispatch (`--auth` vs serve), stderr logging, base-dir resolution
  (`$GSC_MCP_DIR`, else `CARGO_MANIFEST_DIR`).
- `src/auth.rs` — OAuth manager. `token.json` uses google-auth's format/schema.
  Refresh rejections delete the token
  and surface an `auth_error`; the stdio server NEVER opens a browser.
- `src/gsc.rs` — hand-rolled REST client for the ~6 endpoints (webmasters v3 +
  searchconsole v1 urlInspection), with retry-on-5xx/network (4 attempts, exponential backoff).
- `src/analytics.rs` — the helper layer: validation, date resolution, `Row`,
  `execute_search_analytics` (with 300s/256-entry LRU response cache; fresh data never cached),
  `fetch_all_rows` pagination (+probe for exact-cap truncation detection), `merge_compare`,
  `top_by_dimension`, `expected_ctr`. Unit tests live here.
- `src/server.rs` — the MCP surface: tool/prompt routers (rmcp macros), resources implemented
  manually in `ServerHandler`, `get_info` with capabilities.
- `src/prompts.rs` — the 7 prompt texts.

## Gotchas

- **Tools return error dicts, not protocol errors** — `to_result` converts `GscError` into
  `{"error": invalid_input|api_error|auth_error, "message": ...}` inside a successful
  CallToolResult, so agents can read and react to failures. Keep it.
- **Responses are size-capped** — `to_result` runs success payloads through
  `analytics::truncate_for_chat` (port of the Python `_truncate_for_chat`): the largest
  top-level array in an oversized result is trimmed to fit `GSC_MAX_RESPONSE_BYTES`
  (default 120KB, 0 disables), with `response_truncated: true` + `truncation_note`
  (deliberately distinct from the `truncated` flag several tools use for "the upstream
  fetch hit max_rows"). Size is
  measured compact (rmcp's wire format; the Python server measures `indent=2` to match
  FastMCP). A verify-and-shrink loop checks the real payload, not just the estimate.
- **serde_json `preserve_order` feature is on** so result-dict key order matches the Python
  server's insertion order. Don't drop it — the cache key relies on typed-struct field order,
  not map ordering.
- **Rounding**: `round_to` is half-away-from-zero (Python's `round` is banker's) — only exact
  ties differ. `py_round_i64` (used by `expected_ctr`) does implement ties-to-even; keep it.
- **Prompt arguments are strings** (MCP spec): `brand_terms` comma-separated, `days` numeric
  string, parsed with defaults in `src/server.rs`.
- **Sort stability matters** — Python's sorts are stable; all Rust sorts here use stable
  `sort_by` with `total_cmp`. Don't switch to `sort_unstable_by`.
- **`days` is capped at 3650** (`MAX_PERIOD_DAYS`) — untrusted values must never reach
  `chrono::Duration::days`, which panics on overflow.
- **Token refresh failures are classified** — only definitive OAuth grant rejections
  (`invalid_grant` etc.) delete `token.json`; transient 408/429/5xx must not. `TokenFile`
  round-trips unknown fields (`extra` flatten) so google-auth extras like `rapt_token` survive.
- **Body-read failures retry** — a network error while reading a successful response body
  must re-enter the retry loop, never become an empty (and cached) result.
- **rmcp macro order**: `#[tool_router]`/`#[prompt_router]` on separate impl blocks;
  `#[tool_handler] #[prompt_handler]` stacked on the `ServerHandler` impl, which also
  hand-implements resources and `get_info` (the macros skip methods that already exist).

## Credential files (never commit)

- `gsccredentials.json` — OAuth client secrets (Desktop-app client from Google Cloud Console)
- `token.json` — auto-generated/refreshed (google-auth-compatible format)
