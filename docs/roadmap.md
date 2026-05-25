# ROADMAP

Last updated: 2026-04-16

The next-phase execution list for `perfetto-mcp-rs`. The goal is not to pile on more features, but to first close correctness gaps, build up regression-test coverage, and invest in high-value analysis tooling.

> This is a snapshot of intent, not a progress board. Track individual tasks in GitHub issues, so the `- [ ]` state in this file does not drift against an external tracker.

## Principles

- Fix correctness and runtime stability before expanding features
- Close test gaps before refactors and surface-level polish
- Invest in foundations that will be reused over and over
- Keep correctness bugs, runtime hardening, and feature expansion clearly separated

## Priority Order

v0.2 has landed correctness, regression tests, error-model tightening, and download hardening. The current priority order targets v0.3 and beyond:

1. Ship the `stdlib-quickref` MCP Resource (without it, agents fall back to `LIKE '%xxx%'` scans and the remaining domain tools see reduced payoff)
2. Ship `list_stdlib_modules` (enumeration foundation for stdlib-dependent M5 tools)
3. Ship Chrome / Android / CPU domain tools from Milestone 5
4. Pick up `execute_sql` summary modes and schema caching from Milestone 6 once v0.3 is in users' hands

## Milestone 1: Correctness And Runtime Hardening

Goal: remove the issues most likely to make results untrustworthy or destabilize long-running use.

- [x] Fix the instance-identity check in `wait_ready`
  - Current state: `/status` can succeed against an unrelated external process
  - Suggested approach: use a `stderr` startup marker as a readiness gate before entering `/status` polling
  - Acceptance: with the target port pre-occupied, an external process is never identified as our own instance

- [x] Continuously drain and log child-process `stderr`
  - Current state: `stderr` is already `piped()`, but there is no background drain
  - Goal: avoid pipe stalls and improve startup-failure diagnosability
  - Acceptance: startup-failure logs are visible; long-running processes have no risk of filling the pipe

- [x] Make startup and query timeouts configurable
  - Current state: timeout policy is hard-coded
  - Goal: support CLI flags or environment variables
  - Acceptance: startup timeout and HTTP query timeout can both be set explicitly

- [x] Evaluate serializing concurrent spawns for the same path
  - Current state: concurrent `get_client` calls on the same trace can spawn multiple processes and then discard one
  - Goal: avoid pointless duplicate startups
  - Acceptance: concurrent requests for the same path spawn at most one instance

## Milestone 2: Test Hardening

Goal: raise "works right now" to "hard to regress later."

- [x] Add a pure unit test for custom `starting_port` wraparound
  - Goal: verify that after wraparound, allocation returns to `starting_port`
  - Suggestion: extract the port-allocation logic as a pure function to make the boundary testable
  - Acceptance: covers `u16::MAX -> starting_port`

- [x] Add tests for LRU eviction and instance reuse
  - Acceptance: beyond capacity, only the oldest unused instance is evicted

- [x] Add tests for auto-recovery after an abnormal child-process exit
  - Acceptance: after an old instance dies, the next query recovers

- [x] Add concurrent-access tests for same-trace and different-trace paths
  - Acceptance: no deadlocks, no accidental reuse, no duplicate spawns

- [x] Add failure-path tests
  - Scenarios: missing trace, non-executable binary, download failure, port conflict
  - Landed: missing-trace (`get_client_returns_clear_error_for_missing_trace`), non-executable binary (Unix-only `get_client_surfaces_spawn_error_for_non_executable_binary` via `new_with_binary`), download HTTP failure (`download_binary_surfaces_http_5xx_status` against a local 500 responder, also re-verifies URL scrubbing), port conflict (`preflight_port_free_rejects_real_bound_listener` + `allocate_next_port_skips_real_bound_listener` exercising the real probe against a real bound listener)
  - Acceptance: error messages are clear and localizable

- [x] Add regression tests for the server-layer hint logic
  - Current state: relies on `msg.contains(...)`
  - Goal: lock in the current "missing table / missing module" hint behavior
  - Acceptance: tests fire when error wording or classification changes

- [x] Expand e2e fixtures
  - Current state: a single smoke fixture only proves the main path works
  - Landed (subsequently trimmed in the v0.6 pivot): initially shipped `scroll_jank.pftrace`, `page_loads.pftrace`, `event_latency.perfetto-trace`, `histogram.perfetto-trace`, with `tests/e2e_chrome_scroll_jank.rs` driving the removed `chrome_scroll_jank_summary` SQL end-to-end against `scroll_jank.pftrace`. After v0.6, `page_loads.pftrace` and the Chrome domain-tool tests were removed; `scroll_jank.pftrace` is retained and is now exercised by `tests/e2e_stdlib_include.rs::e2e_stdlib_include_chrome_scroll_jank` running the `chrome.scroll_jank.scroll_jank_v3` → `chrome_janky_frames` SQL that `README.md` prints as the canonical migration path.
  - Acceptance: domain tools have representative e2e coverage (met at M2 close; v0.6 redistributes the coverage without losing the Chrome stdlib path)

## Milestone 3: Error Model Tightening

Goal: reduce fragile string matching and make hint logic stable and testable.

- [x] Design `QueryErrorKind`
  - Shipped: `MissingTable`, `MissingModule`, `Other` (marked `#[non_exhaustive]`; `SyntaxError` deferred until a consumer needs it)
  - Acceptance: a clear enum that does not break original error display

- [x] Classify errors closer to the client/decode layer
  - Classification happens once in `decode_query_result`; both server hint formatters now exhaustively match on `QueryErrorKind`
  - Acceptance: hint logic becomes an enum match

- [x] Clean up or consolidate unused error semantics
  - `PerfettoError::NoTraceLoaded` dropped; `QueryError` restructured to a struct variant `{ kind, message }`
  - Acceptance: the error enum matches actual semantics

## Milestone 4: Download And Distribution Hardening

Goal: make install, upgrade, and cache recovery more reliable.

- [x] Switch the download path to "temp file + atomic rename"
  - Landed: stream into `NamedTempFile::new_in(cache_dir)` and `persist` into place; on Windows, retry `PermissionDenied` up to five times with backoff to survive antivirus holding the handle; a single download attempt is capped at a 10-minute wall-clock deadline.
  - Acceptance: an interrupted download does not pollute the cache

- [x] Add checksum or equivalent verification
  - Landed: streaming SHA-256 hasher writes a `trace_processor_shell.sha256` sidecar on save; cache hits re-verify, mismatches redownload, and pre-sidecar caches self-heal in place to support air-gapped upgrades.
  - Acceptance: a corrupted binary is detected and redownloaded

- [x] Add configurable download source / mirror
  - Landed: `--artifacts-base-url` / `PERFETTO_ARTIFACTS_BASE_URL` threaded through `DownloadConfig`; userinfo and query strings are stripped from logs and from `reqwest::Error` via `redact_url` + `without_url`, so mirror tokens cannot leak.
  - Acceptance: networks with restricted access can switch sources

- [x] Strengthen cross-platform CI coverage
  - Landed: CI is split into `lint` + `test`; test runs as a `[ubuntu, macos, windows]` matrix with `fail-fast: false`, and the `trace_processor_shell` cache is deliberately not restored so the full download path runs cold on every PR × OS.
  - At minimum: Linux, macOS, Windows
  - Acceptance: build, test, and release assets all work consistently

## Milestone 5: Productized Analysis Tools

Goal: upgrade from "generic SQL executor" to "analysis tool for common Perfetto scenarios."

### Conventions

- **Tool naming**: `{verb}_{noun}` for utilities (`list_*`, `load_*`, `execute_*`); `{domain}_{metric}_summary` for analysis tools; `_suspects` / `_hotspots` / `_breakdown` accepted where `_summary` does not fit.
- **Description discipline**: every new M5 tool description includes one "USE THIS WHEN" sentence (when the agent should pick it) and one "NEXT STEPS" sentence (what to call after). Borrowed from antarikshc/perfetto-mcp, kept minimal per the single-signal convention.
- **Fixture source**: Android-flavored samples come from `chromium/.../third_party/perfetto/test/data/`, which is GCS-backed — `.sha256` pointers are in-tree, binaries are served publicly at `https://storage.googleapis.com/perfetto/test_data/{filename}-{digest}`. Copy per-tool at implementation time rather than bulk-importing, to limit repo bloat.

### Foundation

- [x] Add `list_stdlib_modules` (shipped v0.7.0)
  - Returns curated JSON array of 10 stdlib modules (chrome / android / generic) with module name, views, description, illustrative usage query
  - Takes no parameters — callable before load_trace, positioned as auxiliary discovery for analyses not covered by the dedicated `chrome_*` tools

### Chrome Tools

> **v0.7 status: five Chrome domain tools shipped as row-preserving thin
> wrappers over PerfettoSQL stdlib views.** v0.5 tried pre-aggregated
> answer-shaped tools; v0.6 reverted on the "answer-shaped" critique; v0.7
> restored the tools with row-level output so agents can group, filter,
> and correlate further on the returned rows. Design principles:
>
> 1. **Curated display, not pure SELECT \***: tool picks a common sort +
>    LIMIT + derived columns (ms unit conversion), opinionated but not
>    analysis-locking.
> 2. **Row-level, not pre-aggregated**: no `GROUP BY` in the tool SQL —
>    agents decide aggregation.
> 3. **Stable public stdlib subset**: only views verified against the
>    vendored Perfetto stdlib source.
>
> Shipped in v0.7.0 (all Chrome-traces-only):
>
> - [x] `chrome_scroll_jank_summary` — row-level `chrome_janky_frames`
>       (cause, sub_cause, delay_since_last_frame, event_latency_id,
>       scroll_id, vsync_interval) sorted by delay DESC, limit 100
> - [x] `chrome_page_load_summary` — FCP / LCP / DCL / load timing per
>       navigation (`chrome.page_loads`)
> - [x] `chrome_main_thread_hotspots` — main-thread tasks > 16ms with
>       cpu_pct, uses `thread.is_main_thread = 1` (caveat: nullable for
>       traces missing thread metadata)
> - [x] `chrome_startup_summary` — startup events with time-to-first-visible
>       (`chrome.startups`)
> - [x] `chrome_web_content_interactions` — clicks / taps / keyboard ranked
>       by duration for INP analysis

- [ ] Add `chrome_frame_timeline_summary`
  - Frame-timeline / jank aggregation using stdlib — look up the exact module name from the vendored stdlib source in the external Chromium checkout at `~/chromium/src/third_party/perfetto/src/trace_processor/perfetto_sql/stdlib/chrome/` per `docs/plans/m5-stdlib-quickref-resource.md:439`; do NOT guess names; `chrome.frame_times` is unverified; tables referenced include `expected_frame_timeline_slice` / `actual_frame_timeline_slice`
  - Acceptance: row-preserving thin wrapper per v0.7 design principles
  - Fixture: `tests/fixtures/scroll_jank.pftrace` (reuse)

- [ ] Add `chrome_blocking_calls_summary`
  - Surfaces `ScopedBlockingCallWithBaseSyncPrimitives` slices (Chrome's sync-IO / sync-wait marker) ranked by thread, process, frequency, and total blocking time. Observed in live sessions on Worker / I/O threads with 15K+ occurrences driving file-mapping and font-load stalls.
  - Acceptance: ranked output, with a clear flag for non-UI threads where blocking is expected (Utility, ThreadPool\*) vs. Worker / Renderer threads where blocking is a latency source
  - Fixture: capture a self-recorded Chrome trace with sync filesystem traffic, or reuse the `trace_file_mapping_small_file` trace referenced in M5 review notes once a sanitized copy is available

### Android Tools

- [ ] Add `android_startup_summary`
  - Key phases of cold / warm app startup
  - Acceptance: total startup duration plus per-phase breakdown
  - Fixture: `api31_startup_cold.perfetto-trace` (small, deterministic, cold-start is the cleanest signal)

- [ ] Add `anr_suspects`
  - Single-pass ranking of main-thread stalls, binder waits, and lock contention; multi-signal root-cause correlation is deferred to a later milestone
  - Acceptance: produces ranked suspects, not raw tables
  - Fixture: `android_anr.pftrace.gz`

- [ ] Add `list_macrobenchmark_slices` **(fixture blocked)**
  - Enumerates `measureBlock` slices with app / test associations, mirroring `com.google.PerfettoMcp`'s `perfetto-list-macrobenchmark-slices`
  - Acceptance: same output shape as the upstream tool
  - Fixture: none in the chromium tree; self-record or capture from AndroidX Benchmark before picking this up. Not v0.3-actionable until a fixture lands.

### Thread-Level Tools (cross-cutting)

- [ ] Add `main_thread_hotspots`
  - Top-N longest main-thread slices per process; common first-drill for ANR / jank investigations. Applies to any trace with thread tracks (Android, Chrome, plain Linux), not Android-only.
  - Acceptance: ranked slice list with process, duration, timestamp
  - Fixture: any Android startup trace (reuse `api31_startup_cold.perfetto-trace`)

### CPU / Memory Tools

- [ ] Add `cpu_hot_threads`
  - High-CPU threads with their processes
  - Acceptance: applicable to common Android / Linux traces
  - Fixture: `android_sched_and_ps.pb` or `example_android_trace_30s.pb`

- [ ] Add `process_cpu_breakdown`
  - Per-process CPU-time distribution, complementary to `cpu_hot_threads`
  - Acceptance: surfaces the heaviest processes first
  - Fixture: same as `cpu_hot_threads`

- [ ] Add `memory_growth_summary`
  - Processes or counters with significant memory growth
  - Acceptance: first-pass filter for memory anomalies
  - Fixture: generic Android trace (no specialized candidate; stretch-goal)

### Supplementary Tools (optional for v0.3)

- [ ] Add `thread_contention_summary`
  - Summarizes `monitor_contention` events — the #1 root cause behind Android ANR / jank
  - Acceptance: ranked contention events with holder / waiter details
  - Fixture: `android_monitor_contention_trace.atr`

- [ ] Add `binder_transaction_summary`
  - Binder IPC latency and transaction counts per interface
  - Acceptance: client / server latency percentiles
  - Fixture: `android_binder_metric_trace.atr`

### MCP Resource

- [x] Expose a stdlib quick-reference as an MCP Resource **(landed v0.15.6-dev)**
  - URI: `resource://perfetto-mcp/stdlib-quickref`
  - Curated table of the most useful stdlib modules with one-line domain hints, complementing `list_stdlib_modules` — the tool enumerates, the resource teaches
  - Inspired by antarikshc/perfetto-mcp's MCP Resources pattern
  - Why P0: live sessions repeatedly show agents falling back to `SELECT DISTINCT cat FROM slice` + `LIKE '%xxx%'` scans when they do not know stdlib modules exist. This resource is a force-multiplier for every subsequent M5 domain tool.
  - Acceptance: agents can retrieve it without an `execute_sql` call

## Milestone 6: Performance And Context Efficiency

Goal: improve the experience with large traces and complex agent workflows.

- [ ] Add limit / summary modes to `execute_sql`
  - Goal: reduce context usage
  - Acceptance: can return column info, the first N rows, total row count, or a summary

- [ ] Evaluate paginated or streaming result output
  - Current state: results are fully decoded into a `Vec<Value>` first
  - Acceptance: a clear decision on keeping the hard cap vs. upgrading to pagination/streaming

- [ ] Cache high-frequency schema queries
  - Scenarios: `list_tables`, `list_table_structure`
  - Acceptance: repeated queries avoid unnecessary RPCs

- [ ] Support query cancellation
  - Scenario: an LLM fires a low-quality long query
  - Acceptance: long queries can be interrupted without waiting for timeout

- [ ] Add tracing spans
  - Goal: make slow queries and high-frequency calls easier to diagnose
  - Acceptance: `sql_len`, duration, `row_count`, and other key fields are observable

## Suggested Release Plan

### v0.2

Focus on stability and correctness.

- [x] Complete `Milestone 1`
- [x] Complete the most critical regression tests from `Milestone 2`
- [x] Complete at least the test hardening or an initial classification scheme from `Milestone 3`
- [x] Complete `Milestone 4`

Release gate:

- [x] No known high-priority correctness bugs in the spawn, query, and reclaim main path
- [x] Stable unit tests
- [x] Stable e2e in CI
- [x] Test coverage for key error hints

### v0.3

Focus on high-value analysis capability and tool discoverability.

- [ ] Ship the `stdlib-quickref` MCP Resource (P0 — every domain tool depends on agents knowing which stdlib modules to `INCLUDE`)
- [ ] Ship `list_stdlib_modules` (foundation for the domain tools below)
- [ ] Ship at least 3 domain tools from `Milestone 5`, spanning at least 2 scenario families (Chrome / Android startup / ANR / CPU / memory)
- [ ] Add fixtures and e2e for each new tool, sourced from the `test/data/` GCS pointers
- [ ] One pass to unify tool descriptions against the `USE THIS WHEN` / `NEXT STEPS` convention

Release gate:

- [ ] `stdlib-quickref` resource is retrievable and covers at least the Chrome and Android stdlib entry points
- [ ] At least 3 new domain tools span at least 2 scenario families
- [ ] README extended with usage examples for the new tools

### v1.0

Focus on "stable, shippable."

- [ ] Complete key runtime hardening
- [ ] Complete the key regression-test matrix
- [ ] Complete cross-platform distribution and diagnostic docs
- [ ] Clear upgrade and compatibility policy

Release gate:

- [ ] Core behavior has regression-test coverage
- [ ] Install and upgrade paths are clear
- [ ] Common failures are diagnosable
- [ ] Enough coverage of typical Perfetto scenarios

## Reference

- Chinese version: `docs/roadmap.zh-CN.md`
