# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

An MCP (Model Context Protocol) server, written in Rust, that wraps Perfetto's
`trace_processor_shell` so LLM agents can run PerfettoSQL queries against trace files
(`.pftrace`, `.perfetto-trace`, `.bin`, …). The server speaks JSON-RPC over stdio via the
`rmcp` crate. It is shipped as a single binary that also self-registers with Claude Code,
Codex, and (manually) Qoder via its own `install` / `uninstall` subcommands.

## Build / test / lint

`protoc` (Protocol Buffers compiler) is required — `build.rs` invokes `prost-build` to
compile `proto/trace_processor.proto`. Install via `apt install protobuf-compiler`,
`brew install protobuf`, or `choco install protoc`. On Unix, if `$HOME/chromium/src/out/Default/protoc`
exists it is auto-used.

```sh
cargo build --release           # release binary at target/release/perfetto-mcp-rs
cargo test --all-targets        # unit + integration tests
cargo test <name>               # single test by substring (e.g. `cargo test e2e_smoke`)
cargo clippy --all-targets -- -D warnings    # CI lint (warnings are errors)
cargo fmt --all --check          # CI format check
```

CI runs with `RUSTFLAGS: -D warnings` on Ubuntu / macOS / Windows. Match that locally before
pushing.

Integration tests under `tests/` actually spawn `trace_processor_shell` against real fixtures
in `tests/fixtures/`. On first run they download the binary (~35 MB, pinned to v54.0) from
the Perfetto LUCI bucket into `dirs::data_local_dir()/perfetto-mcp-rs/<version>/`. To
short-circuit: `export PERFETTO_TP_PATH=/path/to/trace_processor_shell`. CI intentionally does
**not** cache this download — every PR exercises the atomic-persist + Windows-rename-retry path.

To run the server manually (without an MCP client) for ad-hoc debugging:
`RUST_LOG=debug cargo run`. stdout is reserved for JSON-RPC; logs go to stderr.

## Architecture

Five files carry the load. Read them in this order before making non-trivial changes:

- **`src/server.rs`** — defines `PerfettoMcpServer` and every `#[tool]` handler. Holds
  `current_trace: Arc<Mutex<Option<String>>>`: **only** `load_trace` writes it; every other
  tool reads it as the active trace path. There is no `path` parameter on any other tool —
  switching traces means re-calling `load_trace` (cheap because the `trace_processor_shell`
  process is cached). This file is also where the MCP tool descriptions, the per-tool
  parameter structs, and the curated PerfettoSQL stdlib quickref (`STDLIB_INSTRUCTIONS`,
  `STDLIB_MODULE_LIST`) live.

- **`src/tp_manager.rs`** — `TraceProcessorManager` keeps an LRU pool of running
  `trace_processor_shell` child processes (`--max-instances`, default 3), one per
  *canonical* trace path, each bound to a distinct localhost port starting at 9001.
  `kill_on_drop` cleans up evicted instances. Spawn readiness has a two-phase wait: first a
  stderr-marker gate; if no marker arrives within `STATUS_FALLBACK_DELAY`, it falls back to
  polling `/status` and confirming `loaded_trace_name` matches the expected trace (this is
  the instance-identity check — `/status` alone could otherwise succeed against an unrelated
  process on the same port).

- **`src/tp_client.rs`** — thin reqwest client. POSTs protobuf `QueryArgs` to
  `http://127.0.0.1:<port>/query` and GETs `/status`. One client per process.

- **`src/query.rs`** — decodes the `QueryResult` protobuf into `DecodedTable { columns, rows }`.
  **Column order is taken directly from `proto.column_names` and must not be alphabetized**
  (regression-pinned by `e2e_smoke`). Results above `MAX_ROWS = 5000` (`src/error.rs`) return
  `PerfettoError::TooManyRows` instead of being truncated.

- **`src/error.rs`** — `QueryErrorKind::classify` buckets raw `trace_processor_shell` error
  strings into `MissingTable` / `MissingModule` / `MissingColumn` / `Other`. **Casing is the
  discriminant** — SQLite emits `"no such table:"` (lowercase) while Perfetto's stdlib loader
  emits `"Module not found:"` (capital M). Do not `to_lowercase()` the message; classifiers
  downstream rely on the kind to drive LLM-facing hints.

Supporting modules:

- `src/download.rs` — resolution order: `PERFETTO_TP_PATH` env → `which trace_processor_shell`
  → cached download → download from `PERFETTO_ARTIFACTS_BASE_URL` (default LUCI bucket).
  The pinned upstream version is `TP_VERSION = "v54.0"`. URLs are scrubbed
  (`redact_url`) before logging — never echo a presigned or credentialed URL.
- `src/install.rs` — `install` / `uninstall` subcommands. Calls `claude mcp add` (with `--scope`
  user/local/project) and `codex mcp add`; emits a paste-ready JSON block for Qoder (no
  programmatic API). **`--binary-path` is required** — we deliberately do *not* fall back to
  `current_exe()` because on Linux that resolves through `/proc/self/exe`, which would pin a
  versioned install path and break symlink re-point upgrades.
- `src/check_update.rs` — `check-update` subcommand. Exits 0 (up to date / dev build), 2
  (newer release), 1 (network/parse error). Designed for shell-prompt and CI hooks.
- `build.rs` — compiles `proto/trace_processor.proto` via `prost-build`.

## Conventions that matter

**Tool parameter schemas are closed.** Every `Params` struct under `src/server.rs` has
`#[serde(deny_unknown_fields)]`. This is load-bearing: it causes hallucinated fields
(`min_dur_xxxxxxx`, `threshold_ms`, …) to error fast instead of being silently dropped. The
`tool_input_schemas_advertise_closed_object` test pins this across every advertised tool.
Adding a new tool? Carry the attribute.

**Numeric tool params accept JSON-string-of-number too.** Use the `lenient_i64` / `lenient_f64`
/ `lenient_u32` deserializers in `src/server.rs`, not plain `Option<i64>`. Motivated by LLMs
that stringify every numeric argument. The advertised JSON Schema still says
`integer`/`number`; the lenient deserializer is the safety net.

**`current_trace` is only set on success.** `load_trace` must obtain `/status` successfully
before writing `current_trace`. A half-loaded trace must not redirect subsequent tools.

**Prefer the PerfettoSQL stdlib over raw `slice` + `LIKE '%x%'` scans.** The server-level
`instructions` and the `execute_sql` tool description both list curated modules
(`chrome.page_loads`, `chrome.scroll_jank.scroll_jank_v3`, `chrome.tasks`, `slices.with_context`,
…). Keep these in sync if you add/remove modules from `STDLIB_MODULE_LIST`.

**MCP servers must not write to stdout.** stdout is reserved for JSON-RPC. `main.rs` wires
`tracing_subscriber` to stderr — keep it that way.

**Test ports start at 19001+**, not the default 9001, so `cargo test` can run alongside a
live MCP server. Use a unique offset for any new integration test to avoid races with
parallel test runs.

## Releases

`Cargo.toml` is the version source of truth. Release commits follow the format
`release(v0.13.4): <one-line summary>` (see recent git log). `CHANGELOG.md` is hand-maintained.
`docs/roadmap.md` tracks milestone status as a snapshot of intent — individual tasks live in
GitHub issues, not roadmap checkboxes.
