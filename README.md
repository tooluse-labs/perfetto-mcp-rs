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

An [MCP](https://modelcontextprotocol.io) server that lets LLMs analyze
[Perfetto](https://perfetto.dev) traces. Point Claude Code (or any MCP client) at
a trace file (`.pftrace` / `.perfetto-trace` / `.bin` / … — content-sniffed) and
ask in plain language. The server runs PerfettoSQL under the hood, backed by
`trace_processor_shell` — downloaded automatically on first run, no manual
Perfetto install required.

> Dedicated tools ship curated SQL; for custom analysis the agent writes PerfettoSQL — steered toward the right stdlib modules.

## Quick start

You drive perfetto-mcp-rs through an MCP client (Claude Code, Claude Desktop,
Codex, Cursor, …) — install one first if you don't have it.

**1. Install** — downloads the prebuilt binary and, if Claude Code and/or Codex
are present, registers the MCP server automatically:

```sh
# Linux / macOS / Windows (Git Bash, MSYS2, Cygwin)
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh
```
```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
```

Restart Claude Code (or start a new Codex session) to pick it up. Homebrew,
Cargo, project scope, direct-binary download, and manual registration are under
[Install options](#install-options).

**2. Ask in plain language:**

> Load `~/traces/scroll_jank.pftrace` and tell me the top scroll-jank causes.

Swap in any Perfetto trace you have — captured from the Perfetto UI,
`chrome://tracing`, or `record_android_trace`.

The agent calls `load_trace`, sees it's a Chrome trace, and reaches for the
dedicated `chrome_scroll_jank_summary` tool — no SQL to hand-write. When a
question falls outside the dedicated tools, it drops down to `execute_sql` with
raw PerfettoSQL on the same trace.

Works best with agentic clients (Claude Code, Codex, Claude Desktop, Cursor)
that chain multi-turn tool calls and follow the server's error-message nudges.
Non-agentic clients see the same tools and error nudges, but won't chain the
guided flow automatically.

## Tools

Switching traces means re-calling `load_trace` — there is no `path` argument on
the other tools; they all act on the most recently loaded trace.

MCP tool annotations are client-facing intent and safety hints, not server-side
authorization or execution boundaries.

**Essential**

| Tool | Purpose |
|---|---|
| `load_trace` | Open a trace and return a lightweight routing summary (type/profile, duration, platform, process/thread counts, capabilities, redaction policy, recommended next tools) |
| `execute_sql` | Run a PerfettoSQL query (max 5000 returned rows). Prefer the dedicated `chrome_*` / `list_*` tools for standard analyses; use this for custom joins or aggregations they don't expose. Output shaping: `head`/`limit`, `summary`, `columns_only`, `include_row_count`, `max_string_len`. Sensitive URL/header/cookie/path values redacted by default |

**Exploration**

| Tool | Purpose |
|---|---|
| `list_tables` | List tables/views in the loaded trace, optional GLOB filter |
| `list_table_structure` | Show column names and types for a table |
| `list_processes` | List processes (pid, name, start/end timestamps) |
| `list_threads_in_process` | List threads under a process name (up to 2000) |
| `slice_descendants_breakdown` | Summarize child slices under a long slice id without hand-writing recursive CTEs |
| `list_stdlib_modules` | List PerfettoSQL stdlib modules, optional `domain` / `query` / `limit` filters (no trace needed) |

**Chrome traces** — dedicated tools so the agent doesn't hand-roll SQL. Each flags row/string truncation in its metadata.

| Tool | Purpose |
|---|---|
| `chrome_scroll_jank_summary` | Worst janky frames with cause, sub-cause, delay_since_last_frame |
| `chrome_page_load_summary` | Page loads: URL, raw boundary timestamps, FCP, LCP, DCL, load timings (ms) |
| `chrome_page_load_resource_summary` | Compact URL-level resource/request summary for page-load windows, ranked by max overlap with normalized origin, navigation/renderer relatedness, and attribution-scope evidence |
| `chrome_page_load_resource_pipeline` | One URL's lifecycle/request spans joined with background parse, script evaluation, and style/layout signals, plus an evidence boundary for DNS/TLS/TTFB/cache/download hypotheses |
| `chrome_page_load_resource_hotspots` | URL-bearing resource/request slices on thread, process, and async tracks ranked by page-load/window overlap, with process/thread identity where available |
| `chrome_page_load_script_hotspots` | Renderer main-thread script execution grouped by URL/slice/process within a page-load/window, with style/layout descendant signals |
| `chrome_main_thread_hotspots` | Top main-thread tasks by duration with ts, upid/pid, cpu_pct, and optional page-load/time-window filters |
| `chrome_startup_summary` | Browser startup events and time-to-first-visible-content |
| `chrome_web_content_interactions` | Web content interactions (clicks, taps, INP) ranked by duration |

**Resources**

| Resource | Purpose |
|---|---|
| `resource://perfetto-mcp/stdlib-quickref` | On-demand PerfettoSQL stdlib quick reference for Chrome, Android, and generic traces |

## Analyzing a trace

The right path depends on the trace type:

- **Chrome traces** — `load_trace` → dedicated `chrome_*` tools → `execute_sql`
  for deeper cuts on the returned rows. For slow FCP/load, check
  `chrome_page_load_resource_summary` first, then `chrome_page_load_resource_pipeline`
  for one slow URL or `chrome_page_load_resource_hotspots` for slice drilldown,
  before interpreting main-thread `ResourceLoad*` slices as full request time.
  The summary's `resource_timing_evidence` says whether DNS/TLS/TTFB/download/cache
  phase hints exist; keep conclusions at URL lifecycle-span level when phase
  breakdown is absent. Use `chrome_page_load_script_hotspots` for post-resource JS
  and style/layout work, and `slice_descendants_breakdown` on a long task `id`
  for its child-slice breakdown.
- **Other traces (Android, generic)** — `load_trace` → `list_stdlib_modules`
  (or read `resource://perfetto-mcp/stdlib-quickref`) to check for a ready-made
  module first (Android, generic modules like `slices.with_context`), then run it
  via `execute_sql` + `INCLUDE PERFETTO MODULE`. No module fits? Fall back to
  `list_tables` / `list_table_structure` for schema discovery, then `execute_sql`.

**Privacy** — tool results enter the LLM context, and real traces can hold URLs,
headers, cookies, and local paths. `execute_sql` and the dedicated Chrome tools
mask sensitive user and credential-like values by default while keeping the
diagnostic structure visible. For raw forensic work, start the server with
`PERFETTO_MCP_REDACT_STRINGS_DEFAULT=false`; `load_trace` reports the active
policy in its summary.

**Precision** — dedicated Chrome tools preserve full string cells by default. Use
`max_string_len` only when you explicitly want to trade detail for a smaller
response.

## Under the hood: dedicated tool vs. raw SQL

The scroll-jank question above resolves to a single `chrome_scroll_jank_summary`
call — no SQL to write. When you need a cut the dedicated tools don't expose, the
agent drops to `execute_sql` with PerfettoSQL; the same breakdown by hand:

```sql
INCLUDE PERFETTO MODULE chrome.scroll_jank.scroll_jank_v3;
SELECT cause_of_jank, COUNT(*) AS n
FROM chrome_janky_frames
GROUP BY cause_of_jank
ORDER BY n DESC;
```

## Configuration

Server settings are read at startup; when both a CLI flag and environment variable exist, the CLI flag wins.

| Setting (flag / env) | Default | Effect |
|---|---|---|
| `PERFETTO_TP_PATH` | — | Path to an existing `trace_processor_shell` binary; skips auto-download |
| `--startup-timeout-ms` / `PERFETTO_STARTUP_TIMEOUT_MS` | `20000` | Max time to wait for a spawned `trace_processor_shell` to become ready (ms) |
| `--query-timeout-ms` / `PERFETTO_QUERY_TIMEOUT_MS` | `30000` | HTTP timeout for `/status` and `/query` requests (ms) |
| `--max-instances` | `3` | Maximum idle `trace_processor_shell` processes retained in the LRU; active instances stay registered until their queries finish |
| `--span-timings` / `PERFETTO_MCP_SPAN_TIMINGS` | off | Emit tracing span-close timings for performance hotspot diagnosis (`1` / `true` / `yes` / `on`) |
| `--artifacts-base-url` / `PERFETTO_ARTIFACTS_BASE_URL` | LUCI bucket | Override the `trace_processor_shell` download source on a cache miss (mirror/proxy; same pinned version) |
| `PERFETTO_MCP_REDACT_STRINGS_DEFAULT` | `true` | Mask sensitive URL/header/cookie/path strings in tool output; set `false` for raw forensic work |
| `PERFETTO_MCP_FULL_TRACE_FINGERPRINT` | off | Use full-file SHA-256 for trace cache identity instead of head/middle/tail sampling (`1` / `true` / `yes` / `on`) |
| `RUST_LOG` | — | `tracing-subscriber` filter, e.g. `RUST_LOG=debug` for verbose logs (written to stderr) |

## Install options

<details>
<summary>Package managers, Claude scope, direct binary, manual registration</summary>

**Package managers** — if you'd rather not run the install script:

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

If Qoder is detected during a script install, the installer prints a paste-ready
JSON snippet (Qoder has no programmatic MCP-registration API yet — open Qoder
Settings → MCP → + Add and paste).

**Claude scope** — registration defaults to `--scope user` (available from any
directory). For a project-local install, set `SCOPE=local` (or `project`) and run
the script from that project's directory:

```sh
SCOPE=local bash -c 'curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | sh'
```

PowerShell equivalent: `$env:SCOPE = 'local'; irm ... | iex`. Codex has no scope
concept and ignores this variable.

**Direct binary** — supported platforms: linux amd64/arm64, macOS amd64/arm64,
Windows amd64. Grab the binary from the
[releases page](https://github.com/tooluse-labs/perfetto-mcp-rs/releases). Release
assets are named `perfetto-mcp-rs-<platform>` (e.g. `perfetto-mcp-rs-linux-amd64`);
rename or address the downloaded file explicitly when invoking `install`, and on
Unix mark it executable first (`chmod +x`) — the subcommand refuses
non-executable paths to avoid writing a broken MCP entry. Example:

```sh
# Linux amd64 example — adjust the asset name for your platform.
curl -fsSL -o perfetto-mcp-rs \
  https://github.com/tooluse-labs/perfetto-mcp-rs/releases/latest/download/perfetto-mcp-rs-linux-amd64
chmod +x perfetto-mcp-rs
./perfetto-mcp-rs install --scope user --binary-path "$PWD/perfetto-mcp-rs"
```

**Manual MCP client configuration** — if the installer's auto-registration
doesn't apply to your client.

Codex:

```sh
codex mcp add perfetto-rs -- /absolute/path/to/perfetto-mcp-rs
```

JSON-based clients (e.g. Claude Code, Claude Desktop, Cursor):

```json
{
  "mcpServers": {
    "perfetto-rs": {
      "command": "/absolute/path/to/perfetto-mcp-rs"
    }
  }
}
```

</details>

## Upgrade & uninstall

<details>
<summary>Upgrade, pin a version, check for updates, uninstall</summary>

**Upgrade** — run the update subcommand:

```sh
perfetto-mcp-rs update
```

It pulls the latest release, safely overwrites the existing binary (with Windows
file-lock retry), and re-registers the MCP server with Claude Code / Codex
idempotently. No auto-update daemon — upgrades are explicit.

Pin to a specific version with the `--version` flag:

```sh
perfetto-mcp-rs update --version v0.7.0
```

For Claude local/project registrations, re-run from the original project
directory and pass the same scope:

```sh
perfetto-mcp-rs update --scope local
```

The raw installer one-liners still work if you prefer to drive upgrades
manually or need installer-specific environment overrides.

The `VERSION` env var also works, but **must come immediately before `sh`**
(POSIX `VAR=value cmd` only scopes to the next command — `VERSION=v0.7.0 curl
... | sh` puts `VERSION` on `curl`, not on the piped `sh`):

```sh
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.sh | VERSION=v0.7.0 sh
```

PowerShell — set `$env:VERSION` in the same line, since `iex` runs in the current
session:

```powershell
$env:VERSION = 'v0.7.0'; irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/install.ps1 | iex
```

**Check for updates:**

```sh
perfetto-mcp-rs check-update
```

Exits 0 if up to date (or ahead of releases — local dev build), 2 if a newer
release exists, 1 on network or parse error. Useful for shell-prompt integrations
and CI pre-checks. If it reports a newer release, run `perfetto-mcp-rs update`.

**Uninstall** — symmetric one-liner per platform. Deregisters from Claude Code and
Codex, removes the binary, and deletes the cached `trace_processor_shell`.
Idempotent — safe to run if any step was already done by hand.

```sh
# Linux / macOS / Windows (Git Bash, MSYS2, Cygwin)
curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.sh | sh
```
```powershell
# Windows (PowerShell) — close Claude Code, Codex, or anything else using the .exe first
irm https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.ps1 | iex
```

**Scoped installs (local / project)** — `claude` stores local/project entries
keyed by project directory, so uninstall must use the same `SCOPE` AND run from
that directory. Omitting this leaves the scoped Claude entry behind while the
wrapper still removes the binary and cache:

```sh
# Ran `SCOPE=local bash install.sh` in ~/work/foo earlier? Then:
cd ~/work/foo
SCOPE=local bash -c 'curl -fsSL https://raw.githubusercontent.com/tooluse-labs/perfetto-mcp-rs/main/uninstall.sh | sh'
```

PowerShell equivalent: `cd <original-project-dir>; $env:SCOPE = 'local'; irm ... | iex`.

`$INSTALL_DIR` (default `~/.local/bin`) is **not** removed from your PATH:

- **Linux / macOS** — the installer only *prints* a `PATH` hint; if you added it
  to your shell rc, remove that line manually.
- **Windows** — the installer *writes* `$INSTALL_DIR` into your user PATH
  (HKCU\Environment); remove it via System Properties → Environment Variables if
  you want it gone.

Other tools may still depend on this directory, which is why uninstall leaves it
in place.

</details>

## Build from source

<details>
<summary>protoc, cargo build, tests</summary>

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

Development:

```sh
cargo test          # unit tests
cargo clippy        # lint
cargo fmt           # format
```

</details>

## License

Dual-licensed under either of [Apache License, Version 2.0](https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-APACHE) or
[MIT license](https://github.com/tooluse-labs/perfetto-mcp-rs/blob/main/LICENSE-MIT) at your option. Contributions are accepted under
the same terms.
