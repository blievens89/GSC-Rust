# gsc-mcp-rs

A Google Search Console [MCP](https://modelcontextprotocol.io) server in Rust. Single
static binary — no Python, no venv, no Node. **30 tools, 3 resources, 7 prompts** covering
search analytics, SEO analysis, URL inspection, and sitemap management.

## Why Rust?

This started life as a Python MCP server. Porting it to Rust bought:

- **Instant cold start** — MCP clients spawn the server on demand, so startup time is felt
  on every session. A native binary initializes in milliseconds; there's no interpreter or
  import chain to wait on.
- **Nothing to break** — one static binary. No venv to rebuild after an OS update, no
  dependency resolver, no "works until Python upgrades" rot.
- **Lower memory** — a fraction of the resident footprint of the Python process, which adds
  up when several MCP servers run side by side.

(`scripts/bench.py` spawns both servers and measures cold start, per-call latency, and RSS
if you want to reproduce the comparison.)

## Tools

- **Search analytics** — flexible `search_analytics` queries, period comparison, date
  trends, device/country breakdowns, search appearance, landing-page queries, full
  paginated exports (`fetch_all_rows`), multi-property aggregation
- **SEO analysis** — quick wins (high impressions, low CTR), CTR opportunity vs expected
  CTR by position, keyword cannibalization, content decay, trending queries, new/lost
  queries, top growing pages, position distribution, branded vs non-branded performance
- **Bulk export** — `export_search_analytics` pulls all rows the API exposes concurrently and
  writes CSV to disk, returning just the path — for datasets the chat-size cap would trim
- **Inspection & sitemaps** — URL inspection (single + batch), list/submit/delete sitemaps
- **Prompts** — ready-made analysis workflows (site audit, quick-wins triage, and more)

## Setup

### 1. Google Cloud Console (one-time)

You authenticate as your own Google account against your own OAuth app — nothing is
hosted, no keys are shared.

1. In [Google Cloud Console](https://console.cloud.google.com/), create (or pick) a
   project.
2. Enable the **Google Search Console API** (APIs & Services → Library — search
   "Search Console").
3. Configure the **OAuth consent screen** if the project doesn't have one yet
   (APIs & Services → OAuth consent screen): User type **External**, fill in the app
   name and your email, no extra scopes needed. Then add your Google account under
   **Test users**.
4. Create an OAuth client ID of type **Desktop app** (APIs & Services → Credentials →
   Create credentials → OAuth client ID).
5. Download the client JSON and save it as `gsccredentials.json` in this repo's
   directory.

> **Note:** while the consent screen is in *Testing* status, Google expires refresh
> tokens after 7 days — you'll need to rerun `--auth` weekly. Publish the app
> (a click; no verification needed for these scopes on your own account) to get
> long-lived tokens.

### 2. Build and sign in

```bash
cargo build --release          # requires Rust — https://rustup.rs

./target/release/gsc-mcp-rs --auth   # opens a browser; writes token.json
```

The token refreshes automatically. The stdio server never opens a browser itself — if
the grant is revoked, rerun `--auth`.

### 3. Connect

Claude Code:

```bash
claude mcp add gsc -- /path/to/gsc-mcp-rs/target/release/gsc-mcp-rs
```

Or any other MCP client that speaks stdio. Credential files are looked up in the crate
directory by default; set `GSC_MCP_DIR` to keep them elsewhere.

## Development

```bash
cargo test          # unit tests for the analytics helpers
cargo clippy        # lint
cargo build         # debug build
```

## Notes

- `token.json` uses the same format/schema as google-auth's token file, so tokens are
  interchangeable with Python tooling that uses the same OAuth client.
- Tools return structured error dicts (`invalid_input` / `api_error` / `auth_error`)
  inside successful tool results rather than protocol errors, so agents can read and
  react to failures.
- Search-analytics responses are cached for 5 minutes (LRU, 256 entries); `fresh_data`
  bypasses the cache.
- CSV exports are written inside `$GSC_EXPORT_DIR` (default `~/Downloads`); the tool
  refuses absolute paths, `..`, and overwriting existing files.

## License

[MIT](LICENSE)
