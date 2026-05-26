<p align="center">
  <img src="https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/assets/brand/logo-wordmark.svg" width="820" alt="perfetto-mcp-rs logo">
</p>

<p align="center">
  <a href="https://github.com/tooluse-labs/perfetto-mcp-rs/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/tooluse-labs/perfetto-mcp-rs/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/tooluse-labs/perfetto-mcp-rs/releases"><img alt="Release" src="https://img.shields.io/github/v/release/tooluse-labs/perfetto-mcp-rs"></a>
  <a href="https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-MIT"><img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
</p>

<p align="center">
  <strong>English</strong> | <a href="https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/README.zh-CN.md">简体中文</a>
</p>

---

# perfetto-mcp-rs

An [MCP](https://modelcontextprotocol.io) server for analyzing
[Perfetto](https://perfetto.dev) traces with LLMs. Point Claude Code (or any
MCP client) at a Perfetto trace file (`.pftrace` / `.perfetto-trace` / `.bin` / etc. — content-sniffed) and query it with
PerfettoSQL.

Backed by `trace_processor_shell` — downloaded automatically on first run, no
manual Perfetto install required.

Works best with agentic MCP clients (Claude Code, Codex, Claude Desktop, Cursor)
that can chain multi-turn tool calls. Non-agentic clients will see the same
tools but won't be able to follow the error-message nudges that steer the
LLM through the typical `load_trace` → `list_tables` → `list_table_structure` →
`execute_sql` flow.

> Navigate agents toward the right PerfettoSQL stdlib modules — the analysis SQL is always the agent's own.

## Quick install

**One-line install (recommended for all platforms)** — downloads the prebuilt
binary, drops it into `~/.local/bin` (or `%USERPROFILE%\.local\bin` on
Windows), adds it to your user PATH if needed, and — if Claude Code and/or
Codex are installed — registers the MCP server automatically. Restart Claude
Code or start a new Codex session to pick it up. If Qoder is detected, the
installer prints a paste-ready JSON snippet (Qoder has no programmatic
MCP-registration API yet — open Qoder Settings → MCP → + Add and paste).

Linux / macOS / Windows (Git Bash, MSYS2, Cygwin):

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
```

**Alternatives** — if you'd rather install through a package manager:

```sh
# macOS / Linux via Homebrew
brew tap tooluse-labs/tap
brew install perfetto-mcp-rs
# brew prints caveats; run the printed line to register with Claude Code / Codex:
perfetto-mcp-rs install --binary-path "$(brew --prefix)/bin/perfetto-mcp-rs"

# Rust developers via cargo
cargo install --locked perfetto-mcp-rs
perfetto-mcp-rs install --binary-path "$(which perfetto-mcp-rs)"
```

**Claude scope**: registration defaults to `--scope user` (available from any
directory). For a project-local install, set `SCOPE=local` (or `project`) and
run the script from that project's directory:

```sh
SCOPE=local bash -c 'curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh'
```

PowerShell equivalent: `$env:SCOPE = 'local'; irm ... | iex`. Codex has no
scope concept and ignores this variable.

Supported platforms: linux amd64/arm64, macOS amd64/arm64, Windows amd64.
If you'd rather not run a script, grab the binary directly from the
[releases page](https://github.com/tooluse-labs/perfetto-mcp-rs/releases). Release
assets are named `perfetto-mcp-rs-<platform>` (e.g. `perfetto-mcp-rs-linux-amd64`);
rename or address the downloaded file explicitly when invoking `install`, and
on Unix mark it executable first (`chmod +x`) — the subcommand refuses
non-executable paths to avoid writing a broken MCP entry. Example:

```sh
# Linux amd64 example — adjust the asset name for your platform.
curl -fsSL -o perfetto-mcp-rs \
  https://github.com/tooluse-labs/perfetto-mcp-rs/releases/latest/download/perfetto-mcp-rs-linux-amd64
chmod +x perfetto-mcp-rs
./perfetto-mcp-rs install --scope user --binary-path "$PWD/perfetto-mcp-rs"
```

## Upgrade

Re-run the same install command — it pulls the latest release, safely
overwrites the existing binary (with Windows file-lock retry), and
re-registers the MCP server with Claude Code / Codex idempotently.

Pin to a specific version with the `--version` flag:

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh -s -- --version v0.7.0
```

The `VERSION` env var also works, but **must come immediately before `sh`**
(POSIX `VAR=value cmd` only scopes to the next command — `VERSION=v0.7.0 curl
... | sh` puts `VERSION` on `curl`, not on the piped `sh`):

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | VERSION=v0.7.0 sh
```

PowerShell — set `$env:VERSION` in the same line, since `iex` runs in the
current session:

```powershell
$env:VERSION = 'v0.7.0'; irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
```

No auto-update daemon — upgrades are explicit.

## Check for updates

```sh
perfetto-mcp-rs check-update
```

Exits 0 if up to date (or ahead of releases — local dev build), 2 if a newer release exists, 1 on network or parse error. Useful for shell-prompt integrations and CI pre-checks.

## Uninstall

Symmetric one-liner per platform. Deregisters from Claude Code and Codex,
removes the binary, and deletes the cached `trace_processor_shell`. Idempotent
— safe to run if any step was already done by hand.

**Linux / macOS / Windows (Git Bash, MSYS2, Cygwin):**

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.sh | sh
```

**Windows (PowerShell) — close Claude Code, Codex, or anything else using the `.exe` first:**

```powershell
irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.ps1 | iex
```

**Scoped installs (local / project)**: `claude` stores local/project entries
keyed by project directory, so uninstall must use the same `SCOPE` AND run
from that directory. Omitting this leaves the scoped Claude entry behind while
the wrapper still removes the binary and cache:

```sh
# Ran `SCOPE=local bash install.sh` in ~/work/foo earlier? Then:
cd ~/work/foo
SCOPE=local bash -c 'curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.sh | sh'
```

PowerShell equivalent: `cd <original-project-dir>; $env:SCOPE = 'local'; irm ... | iex`.

`$INSTALL_DIR` (default `~/.local/bin`) is **not** removed from your PATH:

- **Linux / macOS** — the installer only *prints* a `PATH` hint; if you added
  it to your shell rc, remove that line manually.
- **Windows** — the installer *writes* `$INSTALL_DIR` into your user PATH
  (HKCU\Environment); remove it via System Properties → Environment Variables
  if you want it gone.

Other tools may still depend on this directory, which is why uninstall leaves
it in place.

## Tools

| Tool | Purpose |
|---|---|
| `load_trace` | Open a Perfetto trace file and return a lightweight routing summary (type/profile, duration, platform, process/thread counts, capabilities, current redaction policy, recommended next tools) |
| `list_tables` | List tables/views in the loaded trace, optional GLOB filter |
| `list_table_structure` | Show column names and types for a table |
| `execute_sql` | Run a PerfettoSQL query (max 5000 rows); optional output shaping supports `head`/`limit`, `summary`, `columns_only`, `include_row_count`, and `max_string_len`; server-side privacy redaction masks sensitive URL/header/cookie/path values by default |
| `list_processes` | List processes in the trace (pid, name, start/end timestamps) |
| `list_threads_in_process` | List threads under a process name (up to 2000) |
| `slice_descendants_breakdown` | Summarize child slices under long slice ids without hand-writing recursive CTEs |
| `chrome_scroll_jank_summary` | Worst janky frames with cause, sub-cause, delay_since_last_frame; metadata flags row/string truncation (Chrome trace) |
| `chrome_page_load_summary` | Page loads: URL, FCP, LCP, DCL, load timings in ms; metadata flags row/string truncation (Chrome trace) |
| `chrome_main_thread_hotspots` | Top main-thread tasks by duration with ts, upid/pid, cpu_pct, and optional page-load/time-window filters; metadata flags row/string truncation (Chrome trace) |
| `chrome_startup_summary` | Browser startup events and time-to-first-visible-content; metadata flags row/string truncation (Chrome trace) |
| `chrome_web_content_interactions` | Web content interactions (clicks, taps, INP) ranked by duration; metadata flags row/string truncation (Chrome trace) |
| `list_stdlib_modules` | List available PerfettoSQL stdlib modules with optional `domain`, `query`, and `limit` filters (no trace needed) |

## Resources

| Resource | Purpose |
|---|---|
| `resource://perfetto-mcp/stdlib-quickref` | On-demand PerfettoSQL stdlib quick reference for Chrome, Android, and generic traces |

Privacy note: MCP tool results are normally visible to the LLM context, and
real traces can contain URLs, headers, cookies, and local paths. `execute_sql`
and the dedicated Chrome tools mask sensitive user and credential-like string
values by default while keeping the diagnostic structure visible. For raw
forensic work, start the server with
`PERFETTO_MCP_REDACT_STRINGS_DEFAULT=false`; `load_trace` reports the active
policy in its summary.

Precision note: dedicated Chrome tools preserve full string cells by default.
Use `max_string_len` only when you explicitly want to trade detail for a
smaller response.

Typical flow depends on trace type:

- **Chrome traces**: `load_trace` → dedicated `chrome_*` tools
  (`chrome_scroll_jank_summary`, `chrome_page_load_summary`,
  `chrome_main_thread_hotspots`, `chrome_startup_summary`,
  `chrome_web_content_interactions`) → `execute_sql` for deeper analysis
  on the returned rows. Use `slice_descendants_breakdown` on a long task `id`
  when you need its child-slice breakdown.
- **Other traces**: `load_trace` → `list_tables` / `list_table_structure`
  for schema discovery → `execute_sql` for queries. Call
  `list_stdlib_modules` or read `resource://perfetto-mcp/stdlib-quickref`
  when stdlib modules might cover your analysis (Android, generic modules
  like `slices.with_context`).

## Example

Ask Claude Code or Codex something like:

> Load `~/traces/scroll_jank.pftrace` and tell me the top scroll jank causes.

Claude will call `load_trace`, then issue a query like:

```sql
INCLUDE PERFETTO MODULE chrome.scroll_jank.scroll_jank_v3;
SELECT cause_of_jank, COUNT(*) AS n
FROM chrome_janky_frames
GROUP BY cause_of_jank
ORDER BY n DESC;
```

## Manual MCP client configuration

If the installer's auto-registration doesn't apply to your client:

**Codex:**

```sh
codex mcp add perfetto-rs -- /absolute/path/to/perfetto-mcp-rs
```

**JSON-based clients (e.g. Claude Code, Claude Desktop, Cursor):**

```json
{
  "mcpServers": {
    "perfetto-rs": {
      "command": "/absolute/path/to/perfetto-mcp-rs"
    }
  }
}
```

## Configuration

| Variable | Effect |
|---|---|
| `PERFETTO_TP_PATH` | Path to an existing `trace_processor_shell` binary; skips auto-download |
| `PERFETTO_STARTUP_TIMEOUT_MS` | Overrides the `trace_processor_shell` startup timeout in milliseconds |
| `PERFETTO_QUERY_TIMEOUT_MS` | Overrides the HTTP status/query timeout in milliseconds |
| `RUST_LOG` | `tracing-subscriber` filter, e.g. `RUST_LOG=debug` for verbose logs (written to stderr) |

CLI flags:

| Flag | Default | Description |
|---|---|---|
| `--max-instances` | 3 | Maximum cached `trace_processor_shell` processes (LRU-evicted) |
| `--startup-timeout-ms` | 20000 | Max time to wait for a spawned `trace_processor_shell` to become ready |
| `--query-timeout-ms` | 30000 | HTTP timeout for `/status` and `/query` requests |

## Build from source

Requires a Rust toolchain and `protoc` (Protocol Buffers compiler):

```sh
# Ubuntu/Debian
sudo apt install -y protobuf-compiler
# macOS
brew install protobuf
# Windows
choco install protoc
```

Then:

```sh
git clone https://github.com/tooluse-labs/perfetto-mcp-rs
cd perfetto-mcp-rs
cargo build --release
# Binary at target/release/perfetto-mcp-rs
```

## Development

```sh
cargo test          # unit tests
cargo clippy        # lint
cargo fmt           # format
```

## License

Dual-licensed under either of [Apache License, Version 2.0](https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-APACHE) or
[MIT license](https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-MIT) at your option. Contributions are accepted under
the same terms.
