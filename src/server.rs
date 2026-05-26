// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        AnnotateAble, ListResourcesResult, PaginatedRequestParams, RawResource,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
        ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::error::{PerfettoError, QueryErrorKind, MAX_ROWS};
use crate::params::*;
use crate::query::DecodedTable;
use crate::sql_templates::*;
use crate::stdlib_catalog::*;
use crate::tp_manager::{loaded_name_matches, strip_size_suffix, TraceProcessorManager};

/// MCP server providing Perfetto trace analysis tools.
///
/// `current_trace` is set by `load_trace` on success and is the **only** path
/// source for every other handler — no other tool accepts an explicit `path`
/// parameter. Switching between multiple cached traces is therefore done by
/// re-calling `load_trace`, which is near-zero-cost when the manager already
/// has a cached `trace_processor_shell` for that path. Overwritten on each
/// successful `load_trace`, so "load A then load B then execute_sql" runs
/// against B.
#[derive(Debug, Clone)]
pub struct PerfettoMcpServer {
    manager: Arc<TraceProcessorManager>,
    current_trace: Arc<Mutex<Option<String>>>,
    tool_router: ToolRouter<Self>,
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PerfettoMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            server_info: rmcp::model::Implementation {
                name: "perfetto-rs".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
                description: Some("MCP server for Perfetto trace analysis".into()),
                icons: None,
                website_url: None,
            },
            capabilities: ServerCapabilities::builder()
                .enable_resources()
                .enable_tools()
                .build(),
            instructions: Some(server_instructions()),
            ..Default::default()
        }
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListResourcesResult::with_all_items(vec![
            stdlib_quickref_resource(),
        ])))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        std::future::ready(match request.uri.as_str() {
            STDLIB_QUICKREF_URI => Ok(ReadResourceResult {
                contents: vec![ResourceContents::TextResourceContents {
                    uri: STDLIB_QUICKREF_URI.to_owned(),
                    mime_type: Some(STDLIB_QUICKREF_MIME_TYPE.to_owned()),
                    text: STDLIB_QUICKREF.to_owned(),
                    meta: None,
                }],
            }),
            uri => Err(McpError::resource_not_found(
                format!("Unknown resource: {uri}"),
                None,
            )),
        })
    }
}

#[tool_router(router = tool_router)]
impl PerfettoMcpServer {
    #[tool(
        name = "load_trace",
        description = "Load a Perfetto trace file for analysis and return a lightweight \
                       routing summary (trace type/profile, duration, platform, process/thread \
                       counts, capabilities, and recommended next tools). Every other tool \
                       operates on the trace set here.\n\
                       \n\
                       Use when: starting any analysis session — call this first.\n\
                       \n\
                       Don't use for: live trace capture (Perfetto records traces; \
                       perfetto-mcp-rs only reads the resulting file) or for streaming \
                       URLs (path must be a complete file on local disk).\n\
                       \n\
                       Parameters: `path` is an absolute path to a Perfetto trace file \
                       (`.pftrace`, `.perfetto-trace`, `.bin`, or any other format \
                       trace_processor accepts — content-sniffed, not by extension). \
                       Calling again with a new path replaces the active \
                       trace; cached `trace_processor_shell` instances make repeat loads \
                       near-zero-cost.\n\
                       \n\
                       Errors when: the file doesn't exist, isn't a valid Perfetto \
                       trace, or `trace_processor_shell` fails to parse it (corrupt \
                       trace, version mismatch). On first run only, also errors if the \
                       `trace_processor_shell` binary fails to download from the \
                       Perfetto LUCI bucket.",
        annotations(
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn load_trace(
        &self,
        Parameters(params): Parameters<LoadTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for(&params.path).await?;

        let status = client
            .status()
            .await
            .map_err(|e| format!("Failed to get status: {e}"))?;

        let display =
            format_loaded_trace_display(&params.path, status.loaded_trace_name.as_deref());

        // Only update current_trace after the client is healthy and status
        // succeeded — a failed load must not redirect subsequent tools to a
        // half-loaded trace. Summary collection below is best-effort and must
        // not turn a successfully loaded trace into a failed load.
        *self.current_trace.lock().await = Some(params.path.clone());

        let summary = collect_load_trace_summary(&client, &params.path).await;

        format_load_trace_response(&display, summary)
    }

    #[tool(
        name = "execute_sql",
        description = "Run a PerfettoSQL query against the loaded trace and return rows as \
                       columnar JSON. Read-only against trace data; SQLite operates \
                       in-memory per session. Aggregates are strongly preferred over raw \
                       row data; results are capped at 5000 rows.\n\
                       \n\
                       Use when: composing analyses not covered by the dedicated tools — \
                       custom aggregations, joins across stdlib modules, or queries against \
                       base tables (`slice`, `thread`, `process`, `sched`).\n\
                       \n\
                       Don't use for: questions the dedicated `chrome_*` tools answer — \
                       they return the same data with the JOIN shape already correct. \
                       Don't hand-roll `slice` scans with `LIKE '%x%'` patterns when a \
                       stdlib module covers the data; `INCLUDE PERFETTO MODULE chrome.tasks` \
                       is faster and the joins are pre-baked.\n\
                       \n\
                       Parameters: `sql` is a single PerfettoSQL statement (the `INCLUDE \
                       PERFETTO MODULE foo;` and `SELECT ...` can be in the same call). \
                       Optional output shaping (`head`/`limit`, `columns_only`, \
                       `summary`, `include_row_count`, `max_string_len`) only \
                       changes what this tool returns; it does not rewrite the \
                       SQL. String results may be redacted by the server privacy \
                       policy before they are returned, preserving diagnostic \
                       structure while masking sensitive URL/header/cookie/path \
                       values. Requires `load_trace` to have run first.\n\
                       \n\
                       Empty `rows` means the query matched nothing — distinct from a SQL \
                       error, which is returned as an error string with a hint pointing \
                       at the most likely cause (missing module, missing column, missing \
                       table).\n\
                       \n\
                       Reference docs (fetch when you need exact column names or function \
                       signatures): \
                       https://perfetto.dev/docs/analysis/stdlib-docs (24 stdlib packages — \
                       chrome / android / sched / slices / linux / wattson / v8 / ...; use \
                       per-package anchors like `#package-chrome`), \
                       https://perfetto.dev/docs/analysis/perfetto-sql-syntax (syntax).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn execute_sql(
        &self,
        Parameters(params): Parameters<ExecuteSqlParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        let table = client
            .query(&params.sql)
            .await
            .map_err(format_execute_sql_error)?;
        format_execute_sql_response(table, &params)
    }

    #[tool(
        name = "list_tables",
        description = "List tables and views in the loaded trace. Read-only.\n\
                       \n\
                       Use when: exploring an unfamiliar trace or verifying a table \
                       exists before writing SQL. Underlying SQL engine is SQLite, \
                       so the catalog tables common in other SQL engines aren't \
                       present — this MCP tool is the schema introspection path.\n\
                       \n\
                       Don't use for: queries against known stdlib modules — go \
                       straight to `execute_sql` with `INCLUDE PERFETTO MODULE`. \
                       Don't reference this tool name inside SQL; it's a separate \
                       MCP tool, not a SQL function — call it via the tool API.\n\
                       \n\
                       Parameters: optional `pattern` — SQLite GLOB filter (e.g. \
                       `chrome_*` for chrome stdlib views, `slice*` for the slice \
                       table family). Without it, internal stdlib tables (`_*`) \
                       are hidden.\n\
                       \n\
                       Empty result: no tables matched the pattern. If a doc-listed \
                       table is missing, retry with an explicit pattern in case \
                       it's marked internal.\n\
                       \n\
                       Errors when: no trace is loaded — call `load_trace` first.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_tables(
        &self,
        Parameters(params): Parameters<ListTablesParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;

        let sql = match &params.pattern {
            Some(pat) => {
                let safe = sanitize_glob_param(pat).map_err(|e| e.to_string())?;
                format!(
                    "SELECT name FROM sqlite_master \
                     WHERE type IN ('table', 'view') AND name GLOB '{safe}' \
                     ORDER BY name"
                )
            }
            // Hide internal stdlib tables (`_*`) — explicit patterns still bypass the filter.
            None => "SELECT name FROM sqlite_master \
                     WHERE type IN ('table', 'view') \
                     AND name NOT LIKE 'sqlite_%' \
                     AND name NOT LIKE '\\_%' ESCAPE '\\' \
                     ORDER BY name"
                .to_owned(),
        };

        let table = client
            .query(&sql)
            .await
            .map_err(|e| format!("Failed to list tables: {e}"))?;

        // SQLite guarantees `sqlite_master.name` is TEXT NOT NULL; surface a
        // non-string as an error rather than silently dropping the row — that
        // would indicate decoder / trace_processor drift worth telling the
        // caller about now that `outputSchema` advertises `names: Vec<String>`.
        let names = table
            .rows
            .into_iter()
            .map(|row| match row.into_iter().next() {
                Some(serde_json::Value::String(s)) => Ok(s),
                other => Err(format!(
                    "Failed to list tables: sqlite_master.name expected TEXT, got {other:?}"
                )),
            })
            .collect::<Result<Vec<_>, String>>()?;

        serde_json::to_string(&TableList { names })
            .map_err(|e| format!("Failed to serialize results: {e}"))
    }

    #[tool(
        name = "list_table_structure",
        description = "Show the columns of a table or view: name, type, nullability, \
                       primary-key flag.\n\
                       \n\
                       Use when: writing or debugging a query — call this immediately \
                       after a `no such column` error to inspect the actual schema \
                       rather than guessing. Both stdlib views and base tables have \
                       fixed schemas; don't infer columns by analogy across them.\n\
                       \n\
                       Don't use for: this is a separate MCP tool, not a SQL function — \
                       don't write `SELECT * FROM list_table_structure` inside \
                       `execute_sql`.\n\
                       \n\
                       Parameters: `table_name` (string) — the exact table or view \
                       name as it appears in `list_tables` output. Case-sensitive; \
                       does not accept GLOB patterns or partial matches. Also \
                       accepts the alias `name` (v0.11.3+).\n\
                       \n\
                       Errors when: the table doesn't exist or has no columns. Call \
                       `list_tables` first if uncertain about the name.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_table_structure(
        &self,
        Parameters(params): Parameters<TableStructureParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        let table_name = sanitize_glob_param(&params.table_name).map_err(|e| e.to_string())?;

        let sql = format!("PRAGMA table_info('{table_name}')");
        let pragma = client
            .query(&sql)
            .await
            .map_err(|e| format!("Failed to get table structure: {e}"))?;

        if pragma.is_empty() {
            return Err(format!("Table '{table_name}' not found or has no columns."));
        }

        let columns = (0..pragma.len())
            .map(|i| pragma_row_to_column_info(&pragma, i))
            .collect::<Result<Vec<_>, String>>()?;

        serde_json::to_string(&TableInfo {
            table: table_name,
            columns,
        })
        .map_err(|e| format!("Failed to serialize results: {e}"))
    }

    #[tool(
        name = "list_processes",
        description = "List every process captured in the trace: upid (trace-internal \
                       id), pid (OS pid), name, start_ts, end_ts. Read-only.\n\
                       \n\
                       Use when: entry point for Android and Linux trace analysis, or \
                       picking the right `pid`/`upid` to feed into `list_threads_in_process` \
                       or `chrome_main_thread_hotspots`.\n\
                       \n\
                       Don't use for: Chrome traces — the dedicated `chrome_*` tools \
                       answer most common questions without process-level navigation.\n\
                       \n\
                       Parameters: none — operates on the loaded trace.\n\
                       \n\
                       Empty result: rare; would mean the trace captured no process \
                       metadata at all.\n\
                       \n\
                       Errors when: no trace is loaded — call `load_trace` first.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_processes(
        &self,
        Parameters(_params): Parameters<ListProcessesParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        let table = client
            .query("SELECT upid, pid, name, start_ts, end_ts FROM process ORDER BY start_ts")
            .await
            .map_err(|e| format!("Failed to list processes: {e}"))?;
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
    }

    #[tool(
        name = "list_threads_in_process",
        description = "List threads inside one process: tid, thread_name, pid, upid. \
                       Limit 2000 rows.\n\
                       \n\
                       Use when: drilling into a specific process picked from \
                       `list_processes` — e.g. finding a renderer's compositor thread, \
                       or auditing all threads under system_server.\n\
                       \n\
                       Don't use for: enumerating ALL threads across the whole trace — \
                       use `execute_sql` against the `thread` table for that.\n\
                       \n\
                       Parameters: pass either `upid` (trace-internal id, precise — \
                       prefer when multiple processes share a name like 'Renderer') or \
                       `process_name` (exact match). `upid` wins when both are set.\n\
                       \n\
                       Empty result: returned as an error pointing at `list_processes` \
                       for available candidates.\n\
                       \n\
                       When the 2000-row cap is hit (system_server, Chrome \
                       renderer-fork): drill down via `execute_sql` against the `thread` \
                       table directly.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_threads_in_process(
        &self,
        Parameters(params): Parameters<ListThreadsInProcessParams>,
    ) -> Result<String, String> {
        // Validate inputs BEFORE opening the trace — failing fast on bad
        // params avoids spawning trace_processor_shell for a request that
        // can't possibly succeed.
        // LIMIT keeps us clear of the 5000-row hard cap on Chrome renderer-fork
        // and Android system_server traces where a single process name can
        // fan out to thousands of threads.
        let (sql, selector_for_error) = match (params.upid, &params.process_name) {
            (Some(upid), _) => (
                format!(
                    "SELECT t.tid, t.name AS thread_name, p.pid, p.upid \
                     FROM thread t JOIN process p ON t.upid = p.upid \
                     WHERE p.upid = {upid} \
                     ORDER BY p.pid, t.tid \
                     LIMIT 2000"
                ),
                format!("upid {upid}"),
            ),
            (None, Some(name)) => {
                let name_lit = sql_string_literal(name).map_err(|e| e.to_string())?;
                (
                    format!(
                        "SELECT t.tid, t.name AS thread_name, p.pid, p.upid \
                         FROM thread t JOIN process p ON t.upid = p.upid \
                         WHERE p.name = {name_lit} \
                         ORDER BY p.pid, t.tid \
                         LIMIT 2000"
                    ),
                    format!("process name {name:?}"),
                )
            }
            (None, None) => {
                return Err("Either `upid` or `process_name` must be provided.".to_string());
            }
        };
        let client = self.client_for_current().await?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format!("Failed to list threads: {e}"))?;
        if table.is_empty() {
            return Err(format!(
                "No threads found for {selector_for_error}. Call list_processes \
                 to see available processes."
            ));
        }
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
    }

    #[tool(
        name = "chrome_scroll_jank_summary",
        description = "Summarize the worst scroll jank frames in a Chrome trace: \
                       cause_of_jank, sub_cause_of_jank, delay_since_last_frame, \
                       event_latency_id, scroll_id, vsync_interval. One row per janky \
                       frame, sorted by delay_since_last_frame DESC, limit 100. \
                       Read-only.\n\
                       \n\
                       Use when: investigating jank reports, finding scroll regressions, \
                       ranking jank causes. Prefer over hand-rolling SQL on \
                       `chrome.scroll_jank.scroll_jank_v3` — same data, less code.\n\
                       \n\
                       Don't use for: non-Chrome traces (will error). For per-frame \
                       causes outside the top 100, drop to `execute_sql` against the \
                       same view.\n\
                       \n\
                       Parameters: optional `max_string_len` caps returned string \
                       cells. Unset preserves full strings for precision. Operates \
                       on the loaded trace.\n\
                       \n\
                       Output: metadata-first JSON preserving `columns` / \
                       `rows`; `truncated=true` means the row cap was reached; \
                       `string_truncated=true` means cell text was shortened.\n\
                       \n\
                       Empty result: no janky frames detected (clean trace) or no \
                       scrolls occurred during capture.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_scroll_jank_summary(
        &self,
        Parameters(params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome scroll jank summary").await?;
        let table = client
            .query(CHROME_SCROLL_JANK_SUMMARY_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome scroll jank summary", e))?;
        format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, params.max_string_len)
    }

    #[tool(
        name = "chrome_page_load_summary",
        description = "Summarize each page navigation in a Chrome trace: navigation id, \
                       URL, FCP / LCP / DCL / load timings in ms. Read-only.\n\
                       \n\
                       Use when: comparing page-load timings across navigations, finding \
                       slow loads, baselining web-vitals before/after a change. Prefer \
                       over hand-joining `chrome.page_loads` — schema is already correct.\n\
                       \n\
                       Don't use for: non-Chrome traces (will error). For sub-event \
                       timings inside one navigation, drop to `execute_sql` against the \
                       `chrome.page_loads` module.\n\
                       \n\
                       Parameters: optional `max_string_len` caps returned string \
                       cells. Unset preserves full strings for precision. Operates \
                       on the loaded trace.\n\
                       \n\
                       Output: metadata-first JSON preserving `columns` / \
                       `rows`; `truncated=true` means the row cap was reached; \
                       `string_truncated=true` means cell text was shortened.\n\
                       \n\
                       Empty result: no navigations occurred during capture (e.g. trace \
                       started after the page was already loaded).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_page_load_summary(
        &self,
        Parameters(params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page load summary").await?;
        let table = client
            .query(CHROME_PAGE_LOAD_SUMMARY_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page load summary", e))?;
        format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, params.max_string_len)
    }

    #[tool(
        name = "chrome_main_thread_hotspots",
        description = "Top Chrome main-thread tasks by wall duration: id, ts, \
                       name, task_type, thread_name, process_name, upid, pid, \
                       dur_ms, cpu_pct (thread_dur/dur), thread_dur_ms. Uses `chrome.tasks`, \
                       `thread.is_main_thread = 1` when available, and Chrome's \
                       `Cr*Main` thread-name convention as a fallback for traces \
                       where thread metadata is incomplete or incorrect. Pass a returned \
                       `id` to `slice_descendants_breakdown` for child-slice breakdowns.\n\
                       \n\
                       Use when: investigating main-thread responsiveness, finding hot \
                       tasks during scroll/load, comparing CPU vs wall time, scoping \
                       to one renderer in multi-renderer traces.\n\
                       \n\
                       Don't use for: non-Chrome traces (will error). For background \
                       (non-main) thread tasks, drop to `execute_sql` against \
                       `chrome.tasks` directly.\n\
                       \n\
                       Parameters (all optional):\n\
                       - `process_name` / `pid` / `upid`: scope to one process or \
                         type. Prefer `upid` for multi-renderer traces; all filters \
                         AND together.\n\
                       - `page_load_id` / `navigation_id` / `phase`: scope to a \
                         page-load window. IDs match `chrome_page_loads.id` and \
                         `.navigation_id` respectively and are mutually exclusive. \
                         `phase`: `navigation_to_fcp`, `navigation_to_load`, \
                         `dcl_to_fcp`, `fcp_to_load`. If an id is set without \
                         `phase`, defaults to `navigation_to_fcp`; phase-only uses \
                         the latest page load.\n\
                       - `start_ts_ns` / `end_ts_ns`: raw trace timestamp bounds \
                         in nanoseconds (`end_ts_ns` exclusive); aliases `start_ts` \
                         / `end_ts` are accepted. These AND with any page-load window.\n\
                       - `min_dur_ms`: minimum task duration. Defaults to 16 (one \
                         60 Hz frame). Pass 0 for ALL tasks; raise to 33 (30 Hz) or \
                         100 to focus on bigger stutters.\n\
                        - `limit`: max rows (default 100, capped at 5000). Must be > 0 \
                          if set.\n\
                        - `max_string_len`: optional cap for returned string cells. \
                          Unset preserves full strings for precision. Must be > 0 if set.\n\
                        \n\
                        Output: metadata-first JSON preserving `columns` / \
                        `rows`; `truncated=true` means the row cap was reached; \
                        `string_truncated=true` means cell text was shortened.\n\
                        \n\
                        Empty result: no detected main-thread tasks exceeded `min_dur_ms` \
                        at the selected process/window threshold, or the trace uses \
                       non-standard main-thread names outside the `Cr*Main` fallback.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_main_thread_hotspots(
        &self,
        Parameters(params): Parameters<ChromeMainThreadHotspotsParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome main-thread hotspots").await?;
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            process_name: params.process_name.as_deref(),
            pid: params.pid,
            upid: params.upid,
            page_load_id: params.page_load_id,
            navigation_id: params.navigation_id,
            phase: params.phase,
            start_ts_ns: params.start_ts_ns,
            end_ts_ns: params.end_ts_ns,
            min_dur_ms: params.min_dur_ms,
            limit: params.limit,
        })
        .map_err(|e| e.to_string())?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome main-thread hotspots", e))?;
        format_chrome_tool_response(
            table,
            chrome_hotspots_effective_limit(params.limit),
            params.max_string_len,
        )
    }

    #[tool(
        name = "slice_descendants_breakdown",
        description = "Recursive child-slice expansion under known `slice.id` roots, \
                       aggregated as a bounded breakdown (slice_count / total_ms / \
                       max_ms per (depth, name) group). Use to drill into a long task \
                       — after `chrome_main_thread_hotspots` or `execute_sql` returns \
                       a slice id — without hand-writing `WITH RECURSIVE` CTEs over \
                       `slice.parent_id`. Required: `slice_ids`. Optional bounds: \
                       `min_dur_ms`, `max_depth`, `limit`, `include_args`, \
                       `max_string_len`. The response echoes `summary_scope`, \
                       `applied_filters`, and `missing_root_ids` (root slice ids \
                       not present in the loaded trace — usually stale ids). \
                       Returned columns: `root_id`, `depth`, `name`, `slice_count`, \
                       `total_ms`, `max_ms`, `first_ts_ns` (raw nanoseconds, not ms), \
                       `example_slice_id` (longest-duration descendant per group), \
                       and optionally `example_args`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn slice_descendants_breakdown(
        &self,
        Parameters(params): Parameters<SliceDescendantsBreakdownParams>,
    ) -> Result<String, String> {
        let max_string_len = tool_max_string_len(params.max_string_len)?;
        let effective_limit =
            slice_descendants_effective_limit(params.limit).map_err(|e| e.to_string())?;
        let applied_filters = slice_descendants_applied_filters(&params, effective_limit);

        let client = self.client_for_current().await?;
        let deduped_root_ids = dedupe_slice_ids_preserving_order(&params.slice_ids);
        let missing_root_ids = if deduped_root_ids.is_empty() {
            Vec::new()
        } else {
            fetch_missing_slice_ids(&client, &deduped_root_ids)
                .await
                .map_err(format_slice_descendants_tool_error)?
        };
        if !deduped_root_ids.is_empty() && missing_root_ids.len() == deduped_root_ids.len() {
            return Err(format!(
                "Failed to run slice_descendants_breakdown: none of the requested \
                 slice_ids exist in the loaded trace ({} missing). The ids are likely \
                 stale, came from a different trace, or refer to thread/process \
                 tracks rather than `slice` rows. Re-run `chrome_main_thread_hotspots` \
                 or `execute_sql` against the current trace to get fresh ids.",
                missing_root_ids.len()
            ));
        }

        let sql = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
            slice_ids: &params.slice_ids,
            min_dur_ms: params.min_dur_ms,
            max_depth: params.max_depth,
            include_args: params.include_args,
            row_limit: effective_limit,
        })
        .map_err(|e| e.to_string())?;

        let table = client
            .query(&sql)
            .await
            .map_err(format_slice_descendants_tool_error)?;
        format_slice_descendants_tool_response_with_redaction(
            table,
            effective_limit as usize,
            applied_filters,
            missing_root_ids,
            max_string_len,
            default_redact_strings(),
        )
    }

    #[tool(
        name = "chrome_startup_summary",
        description = "Summarize Chrome browser startup events: id, name, launch_cause, \
                       startup_duration_ms (first_visible_content_ts - \
                       startup_begin_ts), browser_upid. Read-only.\n\
                       \n\
                       Use when: measuring time-to-first-visible-content for cold \
                       starts, comparing launch causes (NEW_WINDOW vs CMD_LINE vs \
                       RESTORE_SESSION), regressing startup performance.\n\
                       \n\
                       Don't use for: non-Chrome traces (will error). Browser-process \
                       work during steady state is covered by \
                       `chrome_main_thread_hotspots`.\n\
                       \n\
                       Parameters: optional `max_string_len` caps returned string \
                       cells. Unset preserves full strings for precision. Operates \
                       on the loaded trace.\n\
                       \n\
                       Output: metadata-first JSON preserving `columns` / \
                       `rows`; `truncated=true` means the row cap was reached; \
                       `string_truncated=true` means cell text was shortened.\n\
                       \n\
                       Empty result: trace started after the browser was already \
                       running (most cases — startup is captured only when tracing \
                       began before launch).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_startup_summary(
        &self,
        Parameters(params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome startup summary").await?;
        let table = client
            .query(CHROME_STARTUP_SUMMARY_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome startup summary", e))?;
        format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, params.max_string_len)
    }

    #[tool(
        name = "chrome_web_content_interactions",
        description = "Rank web content interactions in a Chrome trace by duration: id, \
                       ts, dur_ms, interaction_type, renderer_upid. Sorted by dur_ms \
                       DESC, limit 100. Read-only.\n\
                       \n\
                       Use when: INP (Interaction to Next Paint) analysis, reproducing \
                       user-felt latency, finding slow click/tap/keyboard handlers.\n\
                       \n\
                       Don't use for: non-Chrome traces (will error). For interactions \
                       outside the top 100 or filtered by `interaction_type`, drop to \
                       `execute_sql` against `chrome.web_content_interactions`.\n\
                       \n\
                       Parameters: optional `max_string_len` caps returned string \
                       cells. Unset preserves full strings for precision. Operates \
                       on the loaded trace.\n\
                       \n\
                       Output: metadata-first JSON preserving `columns` / \
                       `rows`; `truncated=true` means the row cap was reached; \
                       `string_truncated=true` means cell text was shortened.\n\
                       \n\
                       Empty result: no interactions captured (trace started before \
                       user input or interaction tracking was disabled in tracing \
                       config).",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_web_content_interactions(
        &self,
        Parameters(params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome web content interactions").await?;
        let table = client
            .query(CHROME_WEB_CONTENT_INTERACTIONS_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome web content interactions", e))?;
        format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, params.max_string_len)
    }

    #[tool(
        name = "list_stdlib_modules",
        description = "List curated PerfettoSQL stdlib modules as JSON entries with \
                       `domain`, `module`, `views`, `description`, and `usage`. Use \
                       when choosing an `INCLUDE PERFETTO MODULE ...` target; no trace \
                       has to be loaded.\n\
                       \n\
                       Optional filters: `domain` (`chrome`, `android`, `generic`), \
                       `query` (case-insensitive search over module/view/description), \
                       and `limit`. For longer guidance, read \
                       resource://perfetto-mcp/stdlib-quickref.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_stdlib_modules(
        &self,
        Parameters(params): Parameters<ListStdlibModulesParams>,
    ) -> Result<String, String> {
        filtered_stdlib_modules_json(&params)
    }
}

impl PerfettoMcpServer {
    pub fn new(manager: Arc<TraceProcessorManager>) -> Self {
        Self {
            manager,
            current_trace: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// Run the MCP server on stdio transport.
    pub async fn run(self) -> anyhow::Result<()> {
        let transport = rmcp::transport::stdio();
        let service = self.serve(transport).await?;
        service.waiting().await?;
        Ok(())
    }

    /// Return the current trace path set by `load_trace`, or a clear error
    /// directing the caller to `load_trace` when no trace has been loaded.
    async fn current_trace_path(&self) -> Result<String, String> {
        self.current_trace.lock().await.clone().ok_or_else(|| {
            "No trace loaded. Call `load_trace` with an absolute path first.".to_owned()
        })
    }

    /// One-shot "current trace → cached client" used by every non-`load_trace`
    /// handler. Centralizes the two-step preamble so tool descriptions and
    /// future telemetry/retry hooks have one site to wire into.
    async fn client_for_current(&self) -> Result<crate::tp_client::TraceProcessorClient, String> {
        let path = self.current_trace_path().await?;
        self.client_for(&path).await
    }

    /// Resolve a user-provided trace path to a cached client.
    async fn client_for(
        &self,
        trace_path: &str,
    ) -> Result<crate::tp_client::TraceProcessorClient, String> {
        self.manager
            .get_client(Path::new(trace_path))
            .await
            .map_err(|e| format!("Failed to open trace {trace_path:?}: {e}"))
    }
}

fn stdlib_quickref_resource() -> rmcp::model::Resource {
    RawResource {
        uri: STDLIB_QUICKREF_URI.to_owned(),
        name: "stdlib-quickref".to_owned(),
        title: Some("PerfettoSQL stdlib quick reference".to_owned()),
        description: Some(
            "Curated PerfettoSQL stdlib modules and minimal routing examples.".to_owned(),
        ),
        mime_type: Some(STDLIB_QUICKREF_MIME_TYPE.to_owned()),
        size: Some(STDLIB_QUICKREF.len() as u32),
        icons: None,
        meta: None,
    }
    .no_annotation()
}

fn filtered_stdlib_modules_json(params: &ListStdlibModulesParams) -> Result<String, String> {
    if params.domain.is_none() && params.query.is_none() && params.limit.is_none() {
        return Ok(STDLIB_MODULE_LIST.to_owned());
    }

    let domain = params
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    if let Some(domain) = domain.as_deref() {
        if !matches!(domain, "chrome" | "android" | "generic") {
            return Err(format!(
                "`domain` must be one of chrome, android, generic; got {domain:?}"
            ));
        }
    }

    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());

    let limit = match params.limit {
        Some(0) => return Err("`limit` must be > 0 when set.".to_owned()),
        Some(n) => Some(n as usize),
        None => None,
    };

    let modules: Vec<serde_json::Value> = serde_json::from_str(STDLIB_MODULE_LIST)
        .map_err(|e| format!("Failed to parse stdlib module catalog: {e}"))?;
    let iter = modules.into_iter().filter(|entry| {
        let domain_matches = domain
            .as_deref()
            .is_none_or(|domain| entry.get("domain").and_then(|v| v.as_str()) == Some(domain));
        let query_matches = query
            .as_deref()
            .is_none_or(|query| stdlib_module_entry_matches(entry, query));
        domain_matches && query_matches
    });
    let filtered: Vec<_> = match limit {
        Some(limit) => iter.take(limit).collect(),
        None => iter.collect(),
    };

    serde_json::to_string(&filtered).map_err(|e| format!("Failed to serialize results: {e}"))
}

fn stdlib_module_entry_matches(entry: &serde_json::Value, query: &str) -> bool {
    for key in ["domain", "module", "description"] {
        if entry
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.to_ascii_lowercase().contains(query))
        {
            return true;
        }
    }
    entry
        .get("views")
        .and_then(|v| v.as_array())
        .is_some_and(|views| {
            views.iter().any(|view| {
                view.as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().contains(query))
            })
        })
}

/// Hints are kind-gated so unrelated SQL errors don't get misrouted. The
/// MissingColumn hint is intentionally view-agnostic — naming specific
/// stdlib views (e.g. only `chrome_page_loads`) would bias recovery for
/// queries against `slice` / `args` / `thread_state` etc., so the hint
/// names both the stdlib path (`INCLUDE PERFETTO MODULE`) and base tables
/// without favoring either.
fn format_execute_sql_error(err: PerfettoError) -> String {
    match err {
        PerfettoError::QueryError {
            kind: QueryErrorKind::MissingTable,
            message,
        } => format!(
            "SQL error: {message}\n\nHint: Call `list_tables` to find the correct table \
             name, then `list_table_structure` on it before retrying. Stdlib tables \
             (e.g. `chrome_scroll_update_info`) require `INCLUDE PERFETTO MODULE ...;` \
             first."
        ),
        PerfettoError::QueryError {
            kind: QueryErrorKind::MissingColumn,
            message,
        } => format!(
            "SQL error: {message}\n\nHint: Call `list_table_structure('<table>')` \
             against the queried table to see its actual columns. Both stdlib \
             views (anything from `INCLUDE PERFETTO MODULE ...`) and base tables \
             (`slice`, `thread`, `process`, ...) have fixed schemas — avoid \
             inferring column names by analogy."
        ),
        PerfettoError::QueryError { message, .. } => format!("SQL error: {message}"),
        PerfettoError::TooManyRows => format!(
            "Query returned more than {MAX_ROWS} rows. Results should be aggregates \
             rather than raw row data. Reuse stdlib views where possible."
        ),
        other => format!("Query failed: {other}"),
    }
}

const DEFAULT_CHROME_TOOL_ROWS: usize = 100;
const DEFAULT_TOOL_MAX_STRING_LEN: Option<usize> = None;
const DEFAULT_EXECUTE_SQL_SUMMARY_ROWS: usize = 10;
const EXECUTE_SQL_SHAPING_NOTE: &str =
    "row_count is post-SQL decoded rows; head/limit only trims returned tool rows.";
const CHROME_TOOL_SHAPING_NOTE: &str =
    "row_count unknown; truncated=row cap reached; string_truncated=cell text shortened.";
const SLICE_DESCENDANTS_BREAKDOWN_SCOPE: &str = "descendants only; root slices excluded";
const SLICE_DESCENDANTS_SHAPING_NOTE: &str =
    "slice_count and total_ms include only descendants matching min_dur_ms within max_depth; \
     limit caps returned groups; example_slice_id is the longest-duration descendant per group \
     (ties broken by smallest id); first_ts_ns is raw nanoseconds; missing_root_ids lists \
     requested slice_ids that do not exist in the loaded trace; example_args, when present, \
     comes only from example_slice_id.";
const REDACTION_POLICY_NOTE: &str =
    "execute_sql and Chrome dedicated-tool string cells may contain <redacted>; this is server-side policy, not a tool parameter.";

#[derive(Debug, Clone, Serialize, PartialEq)]
struct RedactionPolicy {
    execute_sql_string_cells: bool,
    chrome_tool_string_cells: bool,
    env_var: &'static str,
    note: &'static str,
}

fn redaction_policy_for(enabled: bool) -> RedactionPolicy {
    RedactionPolicy {
        execute_sql_string_cells: enabled,
        chrome_tool_string_cells: enabled,
        env_var: REDACT_STRINGS_DEFAULT_ENV,
        note: REDACTION_POLICY_NOTE,
    }
}

fn current_redaction_policy() -> RedactionPolicy {
    redaction_policy_for(default_redact_strings())
}

fn server_instructions() -> String {
    server_instructions_for_redaction(default_redact_strings())
}

fn server_instructions_for_redaction(enabled: bool) -> String {
    let state = if enabled { "enabled" } else { "disabled" };
    format!(
        "{STDLIB_INSTRUCTIONS}\n\n\
         Privacy policy: SQL/Chrome tool string redaction is {state}. \
         This is server-side policy, not a tool parameter; users control it \
         before startup with {REDACT_STRINGS_DEFAULT_ENV}. If redaction is enabled, \
         execute_sql and Chrome dedicated-tool string cells may contain <redacted> \
         while preserving diagnostic structure."
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecuteSqlOutputMode {
    FullRows,
    LimitedRows(usize),
    Summary(usize),
    ColumnsOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecuteSqlOutputShape {
    mode: ExecuteSqlOutputMode,
    active: bool,
    max_string_len: Option<usize>,
    redact_strings: bool,
}

#[derive(Debug, Serialize)]
struct ExecuteSqlRowsResponse {
    columns: Vec<String>,
    row_count: usize,
    returned_rows: usize,
    truncated: bool,
    row_count_known: bool,
    string_truncated: bool,
    redacted: bool,
    note: &'static str,
    rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
struct ExecuteSqlSummaryResponse {
    columns: Vec<String>,
    row_count: usize,
    returned_rows: usize,
    truncated: bool,
    row_count_known: bool,
    string_truncated: bool,
    redacted: bool,
    note: &'static str,
    sample_rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
struct ExecuteSqlColumnsOnlyResponse {
    columns: Vec<String>,
    row_count: usize,
    returned_rows: usize,
    truncated: bool,
    row_count_known: bool,
    string_truncated: bool,
    redacted: bool,
    note: &'static str,
}

#[derive(Debug, Serialize)]
struct ChromeToolRowsResponse {
    columns: Vec<String>,
    row_count: Option<usize>,
    returned_rows: usize,
    truncated: bool,
    row_count_known: bool,
    string_truncated: bool,
    redacted: bool,
    note: &'static str,
    rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct SliceDescendantsAppliedFilters {
    min_dur_ms: f64,
    max_depth: u32,
    limit: u32,
    include_args: bool,
}

#[derive(Debug, Serialize)]
struct SliceDescendantsRowsResponse {
    columns: Vec<String>,
    row_count: Option<usize>,
    returned_rows: usize,
    truncated: bool,
    row_count_known: bool,
    string_truncated: bool,
    redacted: bool,
    summary_scope: &'static str,
    applied_filters: SliceDescendantsAppliedFilters,
    /// Requested `slice_ids` that do not exist in the loaded trace's `slice`
    /// table. Always present in the response (empty when all roots existed)
    /// so LLM callers can tell "no descendants" apart from "stale id".
    missing_root_ids: Vec<i64>,
    note: &'static str,
    rows: Vec<Vec<serde_json::Value>>,
}

fn chrome_hotspots_effective_limit(limit: Option<u32>) -> usize {
    match limit {
        Some(n) if (n as usize) > MAX_ROWS => MAX_ROWS,
        Some(n) => n as usize,
        None => DEFAULT_CHROME_TOOL_ROWS,
    }
}

fn tool_max_string_len(max_string_len: Option<u32>) -> Result<Option<usize>, String> {
    match max_string_len {
        Some(0) => Err("`max_string_len` must be > 0 when set.".to_owned()),
        Some(n) => Ok(Some(n as usize)),
        None => Ok(DEFAULT_TOOL_MAX_STRING_LEN),
    }
}

fn format_chrome_tool_response(
    table: DecodedTable,
    effective_limit: usize,
    max_string_len: Option<u32>,
) -> Result<String, String> {
    format_chrome_tool_response_with_redaction(
        table,
        effective_limit,
        tool_max_string_len(max_string_len)?,
        default_redact_strings(),
    )
}

fn format_chrome_tool_response_with_redaction(
    table: DecodedTable,
    effective_limit: usize,
    max_string_len: Option<usize>,
    redact_strings: bool,
) -> Result<String, String> {
    let shape = ExecuteSqlOutputShape {
        mode: ExecuteSqlOutputMode::FullRows,
        active: true,
        max_string_len,
        redact_strings,
    };
    let returned_rows = table.rows.len();
    let (rows, string_truncated, redacted) = transform_rows(table.rows.iter(), shape);
    serde_json::to_string(&ChromeToolRowsResponse {
        columns: table.columns,
        row_count: None,
        returned_rows,
        truncated: effective_limit > 0 && returned_rows >= effective_limit,
        row_count_known: false,
        string_truncated,
        redacted,
        note: CHROME_TOOL_SHAPING_NOTE,
        rows,
    })
    .map_err(|e| format!("Failed to serialize results: {e}"))
}

fn slice_descendants_applied_filters(
    params: &SliceDescendantsBreakdownParams,
    effective_limit: u32,
) -> SliceDescendantsAppliedFilters {
    SliceDescendantsAppliedFilters {
        min_dur_ms: params
            .min_dur_ms
            .unwrap_or(DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS),
        max_depth: params
            .max_depth
            .unwrap_or(DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH),
        limit: effective_limit,
        include_args: params.include_args,
    }
}

fn format_slice_descendants_tool_response_with_redaction(
    table: DecodedTable,
    effective_limit: usize,
    applied_filters: SliceDescendantsAppliedFilters,
    missing_root_ids: Vec<i64>,
    max_string_len: Option<usize>,
    redact_strings: bool,
) -> Result<String, String> {
    let shape = ExecuteSqlOutputShape {
        mode: ExecuteSqlOutputMode::FullRows,
        active: true,
        max_string_len,
        redact_strings,
    };
    let returned_rows = table.rows.len();
    let (rows, string_truncated, redacted) = transform_rows(table.rows.iter(), shape);
    serde_json::to_string(&SliceDescendantsRowsResponse {
        columns: table.columns,
        row_count: None,
        returned_rows,
        truncated: effective_limit > 0 && returned_rows >= effective_limit,
        row_count_known: false,
        string_truncated,
        redacted,
        summary_scope: SLICE_DESCENDANTS_BREAKDOWN_SCOPE,
        applied_filters,
        missing_root_ids,
        note: SLICE_DESCENDANTS_SHAPING_NOTE,
        rows,
    })
    .map_err(|e| format!("Failed to serialize results: {e}"))
}

/// Wrapper that re-exports the SQL builder's dedupe under the
/// handler-local name. Keeping the indirection means we always pay for the
/// extra `Vec` copy in one place, and the call site reads naturally.
fn dedupe_slice_ids_preserving_order(ids: &[i64]) -> Vec<i64> {
    dedupe_preserving_order(ids)
}

/// Look up which of the requested root slice ids actually exist in the
/// loaded trace's `slice` table. Returns the list of ids that are NOT
/// present (preserving the caller's order). Empty input → empty output.
///
/// This is intentionally a separate small query rather than baked into the
/// recursive CTE because mixing schemas across a UNION ALL would force
/// every row to carry sentinel NULLs. The per-call cost is a single
/// `id IN (...)` lookup against the indexed `slice.id` column; cheap
/// relative to the recursive expansion that follows.
async fn fetch_missing_slice_ids(
    client: &crate::tp_client::TraceProcessorClient,
    deduped_root_ids: &[i64],
) -> Result<Vec<i64>, PerfettoError> {
    if deduped_root_ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_list = deduped_root_ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT id FROM slice WHERE id IN ({id_list})");
    let table = client.query(&sql).await?;
    let mut found: std::collections::HashSet<i64> =
        std::collections::HashSet::with_capacity(table.len());
    for row_idx in 0..table.len() {
        if let Some(id) = table.cell(row_idx, "id").and_then(|v| v.as_i64()) {
            found.insert(id);
        }
    }
    Ok(deduped_root_ids
        .iter()
        .copied()
        .filter(|id| !found.contains(id))
        .collect())
}

fn format_execute_sql_response(
    table: DecodedTable,
    params: &ExecuteSqlParams,
) -> Result<String, String> {
    format_execute_sql_response_with_redaction(table, params, default_redact_strings())
}

fn format_execute_sql_response_with_redaction(
    table: DecodedTable,
    params: &ExecuteSqlParams,
    redact_strings: bool,
) -> Result<String, String> {
    let shape = execute_sql_output_shape(params, redact_strings)?;
    if !shape.active {
        return serde_json::to_string(&table)
            .map_err(|e| format!("Failed to serialize results: {e}"));
    }

    let row_count = table.rows.len();
    match shape.mode {
        ExecuteSqlOutputMode::ColumnsOnly => {
            serde_json::to_string(&ExecuteSqlColumnsOnlyResponse {
                columns: table.columns,
                row_count,
                returned_rows: 0,
                truncated: false,
                row_count_known: true,
                string_truncated: false,
                redacted: false,
                note: EXECUTE_SQL_SHAPING_NOTE,
            })
            .map_err(|e| format!("Failed to serialize results: {e}"))
        }
        ExecuteSqlOutputMode::Summary(limit) => {
            let (sample_rows, string_truncated, redacted) =
                transform_rows(table.rows.iter().take(limit), shape);
            serde_json::to_string(&ExecuteSqlSummaryResponse {
                columns: table.columns,
                returned_rows: sample_rows.len(),
                sample_rows,
                row_count,
                truncated: row_count > limit,
                row_count_known: true,
                string_truncated,
                redacted,
                note: EXECUTE_SQL_SHAPING_NOTE,
            })
            .map_err(|e| format!("Failed to serialize results: {e}"))
        }
        ExecuteSqlOutputMode::LimitedRows(limit) => {
            let (rows, string_truncated, redacted) =
                transform_rows(table.rows.iter().take(limit), shape);
            serde_json::to_string(&ExecuteSqlRowsResponse {
                columns: table.columns,
                returned_rows: rows.len(),
                rows,
                row_count,
                truncated: row_count > limit,
                row_count_known: true,
                string_truncated,
                redacted,
                note: EXECUTE_SQL_SHAPING_NOTE,
            })
            .map_err(|e| format!("Failed to serialize results: {e}"))
        }
        ExecuteSqlOutputMode::FullRows => {
            let (rows, string_truncated, redacted) = transform_rows(table.rows.iter(), shape);
            serde_json::to_string(&ExecuteSqlRowsResponse {
                columns: table.columns,
                returned_rows: rows.len(),
                rows,
                row_count,
                truncated: false,
                row_count_known: true,
                string_truncated,
                redacted,
                note: EXECUTE_SQL_SHAPING_NOTE,
            })
            .map_err(|e| format!("Failed to serialize results: {e}"))
        }
    }
}

fn execute_sql_output_shape(
    params: &ExecuteSqlParams,
    redact_strings: bool,
) -> Result<ExecuteSqlOutputShape, String> {
    if params.head.is_some() && params.limit.is_some() {
        return Err("`head` and `limit` are aliases; provide only one.".to_owned());
    }
    let row_limit = params.head.or(params.limit);
    if row_limit == Some(0) {
        return Err("`head` / `limit` must be > 0 when set.".to_owned());
    }
    if params.max_string_len == Some(0) {
        return Err("`max_string_len` must be > 0 when set.".to_owned());
    }
    if params.columns_only && params.summary {
        return Err("`columns_only` and `summary` are mutually exclusive.".to_owned());
    }
    if params.columns_only && row_limit.is_some() {
        return Err("`columns_only` cannot be combined with `head` or `limit`.".to_owned());
    }

    let max_string_len = params.max_string_len.map(|n| n as usize);
    let active = params.columns_only
        || params.summary
        || row_limit.is_some()
        || params.include_row_count
        || max_string_len.is_some()
        || redact_strings;
    let mode = if params.columns_only {
        ExecuteSqlOutputMode::ColumnsOnly
    } else if params.summary {
        ExecuteSqlOutputMode::Summary(
            row_limit
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_EXECUTE_SQL_SUMMARY_ROWS),
        )
    } else if let Some(limit) = row_limit {
        ExecuteSqlOutputMode::LimitedRows(limit as usize)
    } else {
        ExecuteSqlOutputMode::FullRows
    };

    Ok(ExecuteSqlOutputShape {
        mode,
        active,
        max_string_len,
        redact_strings,
    })
}

fn transform_rows<'a>(
    rows: impl Iterator<Item = &'a Vec<serde_json::Value>>,
    shape: ExecuteSqlOutputShape,
) -> (Vec<Vec<serde_json::Value>>, bool, bool) {
    let mut any_truncated = false;
    let mut any_redacted = false;
    let transformed = rows
        .map(|row| {
            row.iter()
                .map(|value| {
                    let (value, truncated, redacted) =
                        transform_value(value, shape.max_string_len, shape.redact_strings);
                    any_truncated |= truncated;
                    any_redacted |= redacted;
                    value
                })
                .collect()
        })
        .collect();
    (transformed, any_truncated, any_redacted)
}

fn transform_value(
    value: &serde_json::Value,
    max_string_len: Option<usize>,
    redact_strings: bool,
) -> (serde_json::Value, bool, bool) {
    match value {
        serde_json::Value::String(s) => {
            let (redacted_text, redacted) = if redact_strings {
                redact_string_cell(s)
            } else {
                (s.clone(), false)
            };
            let (text, truncated) = match max_string_len {
                Some(max) => truncate_string_cell(&redacted_text, max),
                None => (redacted_text, false),
            };
            (serde_json::Value::String(text), truncated, redacted)
        }
        serde_json::Value::Array(values) => {
            let mut any_truncated = false;
            let mut any_redacted = false;
            let transformed = values
                .iter()
                .map(|value| {
                    let (value, truncated, redacted) =
                        transform_value(value, max_string_len, redact_strings);
                    any_truncated |= truncated;
                    any_redacted |= redacted;
                    value
                })
                .collect();
            (
                serde_json::Value::Array(transformed),
                any_truncated,
                any_redacted,
            )
        }
        serde_json::Value::Object(map) => {
            let mut any_truncated = false;
            let mut any_redacted = false;
            let transformed = map
                .iter()
                .map(|(key, value)| {
                    let (value, truncated, redacted) =
                        transform_value(value, max_string_len, redact_strings);
                    any_truncated |= truncated;
                    any_redacted |= redacted;
                    (key.clone(), value)
                })
                .collect();
            (
                serde_json::Value::Object(transformed),
                any_truncated,
                any_redacted,
            )
        }
        _ => (value.clone(), false, false),
    }
}

fn truncate_string_cell(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_owned(), false);
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("...<truncated>");
    (out, true)
}

fn redact_string_cell(s: &str) -> (String, bool) {
    let (s, redacted_headers) = redact_sensitive_header_lines(s);
    let (s, redacted_paths) = redact_user_path_segments(&s);
    let (s, redacted_assignments) = redact_sensitive_assignments(&s);
    (
        s,
        redacted_headers || redacted_paths || redacted_assignments,
    )
}

fn redact_sensitive_header_lines(s: &str) -> (String, bool) {
    let mut changed = false;
    let mut out = String::with_capacity(s.len());
    for part in s.split_inclusive('\n') {
        let (line, suffix) = part
            .strip_suffix('\n')
            .map_or((part, ""), |line| (line, "\n"));
        let line_without_cr = line.strip_suffix('\r').unwrap_or(line);
        let cr = if line.ends_with('\r') { "\r" } else { "" };
        let trimmed = line_without_cr.trim_start();
        let leading_len = line_without_cr.len() - trimmed.len();
        let lower = trimmed.to_ascii_lowercase();
        let sensitive = [
            "authorization:",
            "proxy-authorization:",
            "cookie:",
            "set-cookie:",
            "x-api-key:",
            "x-auth-token:",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
        if sensitive {
            let key = trimmed
                .split_once(':')
                .map(|(key, _)| key)
                .unwrap_or(trimmed);
            out.push_str(&line_without_cr[..leading_len]);
            out.push_str(key);
            out.push_str(": <redacted>");
            out.push_str(cr);
            out.push_str(suffix);
            changed = true;
        } else {
            out.push_str(part);
        }
    }
    (out, changed)
}

fn redact_user_path_segments(s: &str) -> (String, bool) {
    let mut out = s.to_owned();
    let mut changed = false;
    for prefix in ["C:\\Users\\", "C:/Users/", "/Users/", "/home/"] {
        let mut search_from = 0;
        while let Some(rel) = out[search_from..].find(prefix) {
            let start = search_from + rel + prefix.len();
            let tail = &out[start..];
            let end = tail
                .char_indices()
                .find(|(_, c)| *c == '\\' || *c == '/')
                .map(|(idx, _)| start + idx)
                .unwrap_or(out.len());
            if start < end && &out[start..end] != "<user>" {
                out.replace_range(start..end, "<user>");
                changed = true;
                search_from = start + "<user>".len();
            } else {
                search_from = (end + 1).min(out.len());
            }
        }
    }
    (out, changed)
}

fn redact_sensitive_assignments(s: &str) -> (String, bool) {
    let mut out = s.to_owned();
    let mut changed = false;
    let sensitive_keys = [
        "access_token",
        "refresh_token",
        "id_token",
        "token",
        "auth",
        "authorization",
        "password",
        "passwd",
        "secret",
        "apikey",
        "api_key",
        "session",
        "sessionid",
        "sid",
        "signature",
        "sig",
        "sign",
        "wpk-header",
        "ud",
        "uid",
        "user_id",
        "userid",
        "device_id",
        "deviceid",
        "open_id",
        "openid",
        "access_key",
        "accesskey",
    ];
    for key in sensitive_keys {
        for separator in ["=", "%3d"] {
            let marker = format!("{key}{separator}");
            changed |= redact_sensitive_assignment_marker(&mut out, &marker, separator == "%3d");
        }
    }
    (out, changed)
}

fn redact_sensitive_assignment_marker(
    out: &mut String,
    marker: &str,
    encoded_marker: bool,
) -> bool {
    let mut changed = false;
    let mut search_from = 0;
    loop {
        if search_from >= out.len() {
            break;
        }
        let lower = out.to_ascii_lowercase();
        let Some(rel) = lower[search_from..].find(marker) else {
            break;
        };
        let key_start = search_from + rel;
        if !has_sensitive_key_boundary(out, key_start, encoded_marker) {
            search_from = key_start + 1;
            continue;
        }

        let value_start = key_start + marker.len();
        let value_end = sensitive_assignment_value_end(out, value_start, encoded_marker);
        if value_start < value_end && &out[value_start..value_end] != "<redacted>" {
            out.replace_range(value_start..value_end, "<redacted>");
            changed = true;
            search_from = (value_start + "<redacted>".len()).min(out.len());
        } else if value_end >= out.len() {
            break;
        } else {
            search_from = value_end + 1;
        }
    }
    changed
}

fn has_sensitive_key_boundary(s: &str, key_start: usize, encoded_marker: bool) -> bool {
    if key_start == 0 {
        return true;
    }

    if encoded_marker && has_ascii_suffix_ignore_case(&s[..key_start], "%26") {
        return true;
    }

    let Some(prev) = s[..key_start].chars().next_back() else {
        return true;
    };
    matches!(
        prev,
        '?' | '&' | ';' | ' ' | '\t' | '\r' | '\n' | '"' | '\'' | '(' | '[' | '{' | '<' | ','
    )
}

fn has_ascii_suffix_ignore_case(s: &str, suffix: &str) -> bool {
    let bytes = s.as_bytes();
    let suffix = suffix.as_bytes();
    bytes.len() >= suffix.len() && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn sensitive_assignment_value_end(s: &str, value_start: usize, encoded_marker: bool) -> usize {
    let tail = &s[value_start..];
    let plain_end = tail
        .char_indices()
        .find(|(_, c)| matches!(*c, '&' | ' ' | '\r' | '\n' | '"' | '\'' | ';'))
        .map(|(idx, _)| value_start + idx);
    let encoded_amp_end = encoded_marker
        .then(|| {
            tail.to_ascii_lowercase()
                .find("%26")
                .map(|idx| value_start + idx)
        })
        .flatten();

    match (plain_end, encoded_amp_end) {
        (Some(plain), Some(encoded)) => plain.min(encoded),
        (Some(plain), None) => plain,
        (None, Some(encoded)) => encoded,
        (None, None) => s.len(),
    }
}

/// Chrome-tool error hint assumes `ensure_chrome_trace` has already rejected
/// non-Chrome traces upstream. So MissingTable here means the expected
/// stdlib view isn't present on a valid Chrome trace (stdlib schema drift
/// across trace_processor_shell versions), and MissingModule means the
/// INCLUDE itself failed (binary lacks the module). Shared by all
/// chrome_* domain tools.
fn format_chrome_tool_error(tool_label: &str, err: PerfettoError) -> String {
    match err {
        PerfettoError::QueryError {
            kind: QueryErrorKind::MissingTable,
            message,
        } => format!(
            "Failed to run {tool_label}: {message}\n\nHint: the expected \
             Chrome stdlib view is not present. This usually indicates \
             trace_processor_shell version drift. Use list_tables to see \
             available views, or check the stdlib schema for the installed \
             trace_processor_shell."
        ),
        PerfettoError::QueryError {
            kind: QueryErrorKind::MissingModule,
            message,
        } => format!(
            "Failed to run {tool_label}: {message}\n\nHint: the required \
             stdlib module is not available in this trace_processor_shell. \
             If PERFETTO_TP_PATH is set, point it at a recent binary; \
             otherwise use execute_sql with a different query."
        ),
        PerfettoError::QueryError {
            kind: QueryErrorKind::MissingColumn,
            message,
        } => format!(
            "Failed to run {tool_label}: {message}\n\nHint: Call \
             `list_table_structure('<table>')` against the queried table to see \
             its actual columns. Both stdlib views (anything from \
             `INCLUDE PERFETTO MODULE ...`) and base tables (`slice`, `thread`, \
             `process`, ...) have fixed schemas — avoid inferring column names \
             by analogy."
        ),
        PerfettoError::QueryError { message, .. } => {
            format!("Failed to run {tool_label}: {message}")
        }
        other => format!("Failed: {other}"),
    }
}

fn format_slice_descendants_tool_error(err: PerfettoError) -> String {
    match err {
        PerfettoError::QueryError {
            kind: QueryErrorKind::MissingTable,
            message,
        } => format!(
            "Failed to run slice_descendants_breakdown: {message}\n\nHint: this tool \
             requires the base `slice` table, and `include_args=true` also requires \
             `args`. Call `list_tables` to verify the trace schema, or use \
             `execute_sql` for a custom fallback query."
        ),
        PerfettoError::QueryError {
            kind: QueryErrorKind::MissingColumn,
            message,
        } => format!(
            "Failed to run slice_descendants_breakdown: {message}\n\nHint: this tool \
             expects modern `slice.id`, `slice.parent_id`, `slice.name`, `slice.dur`, \
             and `slice.ts` columns. Call `list_table_structure('slice')` to inspect \
             the actual schema before retrying with custom SQL."
        ),
        PerfettoError::QueryError { message, .. } => {
            format!("Failed to run slice_descendants_breakdown: {message}")
        }
        other => format!("Failed to run slice_descendants_breakdown: {other}"),
    }
}

/// Preflight check for chrome_* tools. Without it, chrome.* stdlib views
/// on a non-Chrome trace return an empty view (not an error), and each tool
/// would report a successful "no data" outcome, making callers treat the
/// trace as a Chrome trace with no events. This check rejects upfront.
async fn ensure_chrome_trace(
    client: &crate::tp_client::TraceProcessorClient,
    tool_label: &str,
) -> Result<(), String> {
    let table = client
        .query(CHROME_TRACE_PREFLIGHT_SQL)
        .await
        .map_err(|e| format!("{tool_label}: preflight check failed: {e}"))?;
    let has_chrome = table.cell(0, "n").and_then(|v| v.as_i64()).unwrap_or(0);
    if has_chrome == 0 {
        return Err(format!(
            "{tool_label} requires a Chrome-family trace, but no \
             `chrome.process_type` track-descriptor args were found in this \
             trace. Call `list_stdlib_modules` to discover modules that fit \
             this trace, then query via execute_sql."
        ));
    }
    Ok(())
}

/// Project one row of a `PRAGMA table_info('foo')` result into a typed
/// `ColumnInfo`. Surfaces missing `name` / `type` columns as errors —
/// SQLite's PRAGMA contract guarantees them, so absence indicates upstream
/// decoder or trace_processor drift worth surfacing rather than silently
/// rendering a placeholder. `notnull` defaults to 0 (= `nullable: true`)
/// because exotic vtables can legitimately produce NULL there, and
/// "nullable until proven otherwise" is the conservative read.
fn pragma_row_to_column_info(table: &DecodedTable, i: usize) -> Result<ColumnInfo, String> {
    let name = table
        .cell(i, "name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!("PRAGMA table_info row {i} missing `name` column — SQLite contract violation")
        })?
        .to_owned();
    let data_type = table
        .cell(i, "type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!("PRAGMA table_info row {i} missing `type` column — SQLite contract violation")
        })?
        .to_owned();
    let nullable = table
        .cell(i, "notnull")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        == 0;
    Ok(ColumnInfo {
        name,
        data_type,
        nullable,
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct LoadTraceSummary {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_type: Option<String>,
    trace_profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    android_build_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    android_sdk_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chrome_product_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    process_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_count: Option<i64>,
    capabilities: Vec<String>,
    recommended_next_tools: Vec<String>,
    redaction_policy: RedactionPolicy,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

async fn collect_load_trace_summary(
    client: &crate::tp_client::TraceProcessorClient,
    trace_path: &str,
) -> Result<LoadTraceSummary, String> {
    let metadata = client
        .query(LOAD_TRACE_METADATA_SQL)
        .await
        .map_err(|e| format!("metadata query failed: {e}"))?;
    let overview = client
        .query(LOAD_TRACE_OVERVIEW_SQL)
        .await
        .map_err(|e| format!("overview query failed: {e}"))?;
    let file_size_bytes = std::fs::metadata(trace_path).map(|m| m.len()).ok();

    Ok(build_load_trace_summary(
        &metadata,
        &overview,
        file_size_bytes,
    ))
}

fn build_load_trace_summary(
    metadata: &DecodedTable,
    overview: &DecodedTable,
    file_size_bytes: Option<u64>,
) -> LoadTraceSummary {
    let trace_type = metadata_string(metadata, "trace_type");
    let android_build_fingerprint = metadata_string(metadata, "android_build_fingerprint");
    let android_sdk_version = metadata_i64(metadata, "android_sdk_version");
    let chrome_product_version = metadata_string(metadata, "cr-product-version")
        .or_else(|| metadata_string(metadata, "cr-2-product-version"));

    let system_name = metadata_string(metadata, "system_name");
    let system_machine = metadata_string(metadata, "system_machine");
    let chrome_os_name = metadata_string(metadata, "cr-os-name")
        .or_else(|| metadata_string(metadata, "cr-2-os-name"));
    let platform = infer_platform(
        android_build_fingerprint.as_deref(),
        android_sdk_version,
        chrome_os_name.as_deref(),
        system_name.as_deref(),
        system_machine.as_deref(),
    );

    let start_ts = overview_i64(overview, "start_ts");
    let end_ts = overview_i64(overview, "end_ts");
    let duration_ns = overview_i64(overview, "duration_ns")
        .filter(|duration_ns| *duration_ns >= 0)
        .or_else(|| match (start_ts, end_ts) {
            (Some(start), Some(end)) if end >= start => Some(end - start),
            _ => None,
        });

    let has_chrome = overview_bool(overview, "has_chrome");
    let is_android = android_build_fingerprint.is_some()
        || android_sdk_version.is_some()
        || chrome_os_name.as_deref() == Some("Android");
    let trace_profile = if has_chrome {
        "chrome"
    } else if is_android {
        "android"
    } else if trace_type.is_some() || start_ts.is_some() || end_ts.is_some() {
        "generic"
    } else {
        "unknown"
    }
    .to_owned();

    let mut capabilities = Vec::new();
    push_capability(&mut capabilities, has_chrome, "chrome");
    push_capability(&mut capabilities, is_android, "android");
    push_capability(
        &mut capabilities,
        overview_bool(overview, "has_sched"),
        "sched",
    );
    push_capability(
        &mut capabilities,
        overview_bool(overview, "has_ftrace"),
        "ftrace",
    );
    push_capability(
        &mut capabilities,
        overview_bool(overview, "has_slices"),
        "slices",
    );
    push_capability(
        &mut capabilities,
        overview_bool(overview, "has_counters"),
        "counters",
    );

    let recommended_next_tools = recommended_tools(&trace_profile);
    let mut warnings = Vec::new();
    if duration_ns.is_none() {
        warnings.push("trace duration unavailable from trace_dur()".to_owned());
    }
    if overview_i64(overview, "process_count").is_none() {
        warnings.push("process count unavailable".to_owned());
    }
    if overview_i64(overview, "thread_count").is_none() {
        warnings.push("thread count unavailable".to_owned());
    }

    LoadTraceSummary {
        available: true,
        trace_type,
        trace_profile,
        platform,
        android_build_fingerprint,
        android_sdk_version,
        chrome_product_version,
        start_ts,
        end_ts,
        duration_ms: duration_ns.map(ns_to_ms),
        file_size_bytes,
        process_count: overview_i64(overview, "process_count"),
        thread_count: overview_i64(overview, "thread_count"),
        capabilities,
        recommended_next_tools,
        redaction_policy: current_redaction_policy(),
        warnings,
    }
}

fn format_load_trace_response(
    display: &str,
    summary: Result<LoadTraceSummary, String>,
) -> Result<String, String> {
    let summary_json = match summary {
        Ok(summary) => serde_json::to_string(&summary)
            .map_err(|e| format!("Failed to serialize load summary: {e}"))?,
        Err(error) => serde_json::to_string(&serde_json::json!({
            "available": false,
            "error": error,
            "redaction_policy": current_redaction_policy(),
        }))
        .map_err(|e| format!("Failed to serialize load summary: {e}"))?,
    };

    Ok(format!(
        "Trace loaded successfully: {display}\n\
         Trace summary: {summary_json}\n\
         Routing hint: use `recommended_next_tools` from the summary first; \
         use `list_tables` / `list_table_structure` for schema discovery and \
         `execute_sql` for custom PerfettoSQL."
    ))
}

fn metadata_string(table: &DecodedTable, key: &str) -> Option<String> {
    for row_idx in 0..table.len() {
        if table.cell(row_idx, "name").and_then(|v| v.as_str()) == Some(key) {
            return table
                .cell(row_idx, "str_value")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
        }
    }
    None
}

fn metadata_i64(table: &DecodedTable, key: &str) -> Option<i64> {
    for row_idx in 0..table.len() {
        if table.cell(row_idx, "name").and_then(|v| v.as_str()) == Some(key) {
            return table.cell(row_idx, "int_value").and_then(|v| v.as_i64());
        }
    }
    None
}

fn overview_i64(table: &DecodedTable, column: &str) -> Option<i64> {
    table.cell(0, column).and_then(|v| v.as_i64())
}

fn overview_bool(table: &DecodedTable, column: &str) -> bool {
    overview_i64(table, column).unwrap_or(0) != 0
}

fn infer_platform(
    android_build_fingerprint: Option<&str>,
    android_sdk_version: Option<i64>,
    chrome_os_name: Option<&str>,
    system_name: Option<&str>,
    system_machine: Option<&str>,
) -> Option<String> {
    if android_build_fingerprint.is_some()
        || android_sdk_version.is_some()
        || chrome_os_name == Some("Android")
    {
        return Some("Android".to_owned());
    }

    let system_name = system_name.filter(|s| !s.is_empty())?;
    match system_machine.filter(|s| !s.is_empty()) {
        Some(machine) => Some(format!("{system_name} ({machine})")),
        None => Some(system_name.to_owned()),
    }
}

fn push_capability(capabilities: &mut Vec<String>, enabled: bool, name: &str) {
    if enabled {
        capabilities.push(name.to_owned());
    }
}

fn recommended_tools(trace_profile: &str) -> Vec<String> {
    let tools = match trace_profile {
        "chrome" => [
            "chrome_page_load_summary",
            "chrome_scroll_jank_summary",
            "chrome_main_thread_hotspots",
            "chrome_web_content_interactions",
            "list_processes",
            "execute_sql",
        ]
        .as_slice(),
        "android" => [
            "list_stdlib_modules",
            "list_processes",
            "list_threads_in_process",
            "execute_sql",
        ]
        .as_slice(),
        _ => [
            "list_tables",
            "list_table_structure",
            "list_processes",
            "execute_sql",
        ]
        .as_slice(),
    };

    tools.iter().map(|tool| (*tool).to_owned()).collect()
}

fn ns_to_ms(ns: i64) -> f64 {
    ((ns as f64 / 1_000_000.0) * 1000.0).round() / 1000.0
}

/// Render the load confirmation. If `trace_processor_shell`'s `/status` reports
/// a name that differs from the filesystem path we loaded — typically because
/// the trace's recording embedded a different name — surface both so users do
/// not mistake it for the wrong file loading.
fn format_loaded_trace_display(trace_path: &str, loaded_trace_name: Option<&[u8]>) -> String {
    let Some(loaded) = loaded_trace_name else {
        return trace_path.to_string();
    };
    if loaded_name_matches(loaded, Path::new(trace_path)) {
        trace_path.to_string()
    } else {
        let loaded_lossy = String::from_utf8_lossy(loaded);
        format!(
            "{trace_path} (recorded as '{}')",
            strip_size_suffix(&loaded_lossy)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn format_loaded_trace_display_shows_only_path_when_name_matches() {
        assert_eq!(
            format_loaded_trace_display("/tmp/trace.pftrace", Some(b"/tmp/trace.pftrace")),
            "/tmp/trace.pftrace"
        );
        assert_eq!(
            format_loaded_trace_display("/tmp/trace.pftrace", Some(b"trace.pftrace")),
            "/tmp/trace.pftrace"
        );
        assert_eq!(
            format_loaded_trace_display("/tmp/trace.pftrace", Some(b"/tmp/trace.pftrace (12 MB)")),
            "/tmp/trace.pftrace"
        );
    }

    #[test]
    fn format_loaded_trace_display_normalizes_windows_paths() {
        assert_eq!(
            format_loaded_trace_display(
                "C:\\Users\\admin\\trace.gz",
                Some(b"C:/Users/admin/trace.gz")
            ),
            "C:\\Users\\admin\\trace.gz"
        );
    }

    #[test]
    fn format_loaded_trace_display_surfaces_embedded_recording_name() {
        assert_eq!(
            format_loaded_trace_display(
                "C:\\Users\\admin\\trace_pdf.json.gz",
                Some(b"scroll_jank.pftrace")
            ),
            "C:\\Users\\admin\\trace_pdf.json.gz (recorded as 'scroll_jank.pftrace')"
        );
    }

    #[test]
    fn format_loaded_trace_display_falls_back_when_status_has_no_name() {
        assert_eq!(
            format_loaded_trace_display("/tmp/trace.pftrace", None),
            "/tmp/trace.pftrace"
        );
    }

    /// Regression test for v0.8.7 → v0.8.8: trace_processor on a CJK-locale
    /// Windows host echoes the argv path bytes raw in `/status`. Those
    /// bytes are cp936-encoded (e.g. `低端机` → `\xb5\xcd\xb6\xcb\xbb\xfa`)
    /// and not valid UTF-8 — but the basename is ASCII and survives
    /// `String::from_utf8_lossy`. The path-suffix-on-basename match must
    /// accept these mojibake'd directory paths. Forward slashes on both
    /// sides keep `Path::file_name()` portable across Unix/Windows CI;
    /// the real-world Windows path uses `\` but the matcher already
    /// normalizes both sides through `normalize_status_path`.
    #[test]
    fn format_loaded_trace_display_matches_when_cjk_dir_arrives_as_cp936() {
        let loaded: &[u8] =
            b"C:/Users/admin/Downloads/\xb5\xcd\xb6\xcb\xbb\xfatraces/round13_2_trace.bin (28 MB)";
        assert_eq!(
            format_loaded_trace_display(
                "C:/Users/admin/Downloads/低端机traces/round13_2_trace.bin",
                Some(loaded),
            ),
            "C:/Users/admin/Downloads/低端机traces/round13_2_trace.bin",
            "basename match must rescue the CJK-locale mojibake'd path"
        );
    }

    fn decoded_table(columns: &[&str], rows: Vec<Vec<serde_json::Value>>) -> DecodedTable {
        DecodedTable {
            columns: columns.iter().map(|column| (*column).to_owned()).collect(),
            rows,
        }
    }

    fn execute_sql_params(sql: &str) -> ExecuteSqlParams {
        ExecuteSqlParams {
            sql: sql.to_owned(),
            limit: None,
            head: None,
            columns_only: false,
            summary: false,
            include_row_count: false,
            max_string_len: None,
        }
    }

    fn assert_json_key_order(response: &str, first: &str, second: &str) {
        let first_pos = response
            .find(first)
            .unwrap_or_else(|| panic!("missing key fragment {first:?} in {response}"));
        let second_pos = response
            .find(second)
            .unwrap_or_else(|| panic!("missing key fragment {second:?} in {response}"));
        assert!(
            first_pos < second_pos,
            "expected {first:?} before {second:?}, got: {response}"
        );
    }

    #[test]
    fn load_trace_summary_classifies_android_chrome_trace() {
        let metadata = decoded_table(
            &["name", "str_value", "int_value"],
            vec![
                vec![json!("trace_type"), json!("proto"), serde_json::Value::Null],
                vec![
                    json!("android_build_fingerprint"),
                    json!("google/oriole/oriole:13/TP1A.220624.014/8819323:userdebug/dev-keys"),
                    serde_json::Value::Null,
                ],
                vec![
                    json!("android_sdk_version"),
                    serde_json::Value::Null,
                    json!(33),
                ],
                vec![
                    json!("cr-product-version"),
                    json!("Chrome/121.0.6167.178"),
                    serde_json::Value::Null,
                ],
            ],
        );
        let overview = decoded_table(
            &[
                "start_ts",
                "end_ts",
                "duration_ns",
                "process_count",
                "thread_count",
                "has_slices",
                "has_counters",
                "has_sched",
                "has_ftrace",
                "has_chrome",
            ],
            vec![vec![
                json!(1_000),
                json!(2_001_000),
                json!(2_000_000),
                json!(5),
                json!(14),
                json!(1),
                json!(1),
                json!(1),
                json!(1),
                json!(1),
            ]],
        );

        let summary = build_load_trace_summary(&metadata, &overview, Some(6_392_331));

        assert!(summary.available);
        assert_eq!(summary.trace_type.as_deref(), Some("proto"));
        assert_eq!(summary.trace_profile, "chrome");
        assert_eq!(summary.platform.as_deref(), Some("Android"));
        assert_eq!(summary.android_sdk_version, Some(33));
        assert_eq!(
            summary.chrome_product_version.as_deref(),
            Some("Chrome/121.0.6167.178")
        );
        assert_eq!(summary.duration_ms, Some(2.0));
        assert_eq!(summary.process_count, Some(5));
        assert_eq!(summary.thread_count, Some(14));
        assert_eq!(
            summary.capabilities,
            vec!["chrome", "android", "sched", "ftrace", "slices", "counters"]
        );
        assert!(
            summary
                .recommended_next_tools
                .contains(&"chrome_main_thread_hotspots".to_owned()),
            "Chrome summary must route callers to dedicated chrome tools: {summary:?}",
        );
        assert!(summary.warnings.is_empty());
    }

    #[test]
    fn load_trace_response_embeds_parseable_summary_json() {
        let summary = LoadTraceSummary {
            available: true,
            trace_type: Some("proto".to_owned()),
            trace_profile: "generic".to_owned(),
            platform: Some("Linux (x86_64)".to_owned()),
            android_build_fingerprint: None,
            android_sdk_version: None,
            chrome_product_version: None,
            start_ts: Some(10),
            end_ts: Some(20),
            duration_ms: Some(0.00001),
            file_size_bytes: Some(207),
            process_count: Some(4),
            thread_count: Some(4),
            capabilities: vec!["slices".to_owned()],
            recommended_next_tools: recommended_tools("generic"),
            redaction_policy: redaction_policy_for(true),
            warnings: vec![],
        };

        let response = format_load_trace_response("/tmp/basic.perfetto-trace", Ok(summary))
            .expect("response must serialize");
        let summary_line = response
            .lines()
            .find_map(|line| line.strip_prefix("Trace summary: "))
            .expect("response must contain a Trace summary line");
        let parsed: serde_json::Value =
            serde_json::from_str(summary_line).expect("summary line must be JSON");

        assert_eq!(parsed["available"], json!(true));
        assert_eq!(parsed["trace_profile"], json!("generic"));
        assert_eq!(parsed["process_count"], json!(4));
        assert!(
            response.contains("recommended_next_tools"),
            "response should expose routing data, got: {response}",
        );
        assert_eq!(
            parsed["redaction_policy"]["execute_sql_string_cells"],
            json!(true),
            "response should expose current privacy policy, got: {response}",
        );
        assert_eq!(
            parsed["redaction_policy"]["chrome_tool_string_cells"],
            json!(true),
            "response should expose Chrome tool privacy policy, got: {response}",
        );
        assert_eq!(
            parsed["redaction_policy"]["env_var"],
            json!(REDACT_STRINGS_DEFAULT_ENV)
        );
    }

    #[test]
    fn load_trace_response_preserves_success_when_summary_unavailable() {
        let response = format_load_trace_response("/tmp/basic.perfetto-trace", Err("boom".into()))
            .expect("response must serialize");
        let summary_line = response
            .lines()
            .find_map(|line| line.strip_prefix("Trace summary: "))
            .expect("response must contain a Trace summary line");
        let parsed: serde_json::Value =
            serde_json::from_str(summary_line).expect("summary line must be JSON");

        assert_eq!(parsed["available"], json!(false));
        assert_eq!(parsed["error"], json!("boom"));
        assert!(
            parsed.get("redaction_policy").is_some(),
            "summary failure must still expose privacy policy: {response}",
        );
        assert!(
            response.starts_with("Trace loaded successfully"),
            "summary failure must not make load_trace look failed: {response}",
        );
    }

    #[test]
    fn load_trace_returns_summary_from_real_trace() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        runtime.block_on(async {
            let manager = Arc::new(TraceProcessorManager::new_with_starting_port(1, 19_061));
            let server = PerfettoMcpServer::new(manager);
            let response = server
                .load_trace(Parameters(LoadTraceParams {
                    path: "tests/fixtures/basic.perfetto-trace".to_owned(),
                }))
                .await
                .expect("load_trace on basic fixture must succeed");

            let summary_line = response
                .lines()
                .find_map(|line| line.strip_prefix("Trace summary: "))
                .expect("load_trace must include a summary line");
            let parsed: serde_json::Value =
                serde_json::from_str(summary_line).expect("summary line must be JSON");

            assert_eq!(parsed["available"], json!(true));
            assert_eq!(parsed["trace_type"], json!("proto"));
            assert_eq!(parsed["trace_profile"], json!("generic"));
            assert_eq!(parsed["process_count"], json!(4));
            assert_eq!(parsed["thread_count"], json!(4));
            assert!(
                parsed["duration_ms"].as_f64().is_some(),
                "trace_dur() duration should be present: {parsed}",
            );
            assert!(
                parsed["recommended_next_tools"]
                    .as_array()
                    .expect("recommended_next_tools must be an array")
                    .iter()
                    .any(|tool| tool.as_str() == Some("execute_sql")),
                "summary should preserve execute_sql as an escape hatch: {parsed}",
            );
        });
    }

    #[test]
    fn execute_sql_hint_fires_on_missing_table() {
        let formatted = format_execute_sql_error(PerfettoError::QueryError {
            kind: QueryErrorKind::MissingTable,
            message: "no such table: foo".to_owned(),
        });
        assert!(
            formatted.contains("Hint:"),
            "missing-table errors must surface a hint, got: {formatted}",
        );
        assert!(
            formatted.contains("list_tables"),
            "hint must point at list_tables, got: {formatted}",
        );
        assert!(
            formatted.contains("INCLUDE PERFETTO MODULE"),
            "hint must mention the stdlib include directive, got: {formatted}",
        );
    }

    /// MissingColumn hint must be view-agnostic — naming specific stdlib views
    /// (e.g. `chrome_page_loads`) would bias recovery for queries against base
    /// tables like `slice` / `args`. The negative assertion is the bias guard.
    #[test]
    fn execute_sql_hint_fires_on_missing_column() {
        let formatted = format_execute_sql_error(PerfettoError::QueryError {
            kind: QueryErrorKind::MissingColumn,
            message: "no such column: navigation_id".to_owned(),
        });
        assert!(
            formatted.contains("Hint:"),
            "missing-column errors must surface a hint, got: {formatted}",
        );
        assert!(
            formatted.contains("list_table_structure"),
            "hint must point at list_table_structure, got: {formatted}",
        );
        assert!(
            formatted.contains("INCLUDE PERFETTO MODULE"),
            "hint must mention the stdlib path, got: {formatted}",
        );
        assert!(
            formatted.contains("slice"),
            "hint must name at least one base table, got: {formatted}",
        );
        assert!(
            !formatted.contains("chrome_page_loads") && !formatted.contains("chrome_tasks"),
            "hint must NOT name specific stdlib views — that biases recovery for \
             non-Chrome queries; got: {formatted}",
        );
    }

    #[test]
    fn execute_sql_hint_skips_unrelated_query_errors() {
        let formatted = format_execute_sql_error(PerfettoError::QueryError {
            kind: QueryErrorKind::Other,
            message: "syntax error near WHERE".to_owned(),
        });
        assert!(
            !formatted.contains("Hint:"),
            "unrelated SQL errors must not get the missing-table hint, got: {formatted}",
        );
        assert!(
            formatted.contains("syntax error"),
            "unrelated errors must still surface the original message, got: {formatted}",
        );
    }

    #[test]
    fn execute_sql_too_many_rows_message_explains_aggregation() {
        let formatted = format_execute_sql_error(PerfettoError::TooManyRows);
        assert!(
            formatted.contains("5000"),
            "row-cap message must name the limit, got: {formatted}",
        );
        assert!(
            formatted.contains("aggregate"),
            "row-cap message must push agents toward aggregation, got: {formatted}",
        );
    }

    #[test]
    fn execute_sql_response_without_shaping_preserves_legacy_shape() {
        let table = decoded_table(&["a"], vec![vec![json!("abcdef")]]);
        let params = execute_sql_params("SELECT 'abcdef' AS a");

        let response =
            format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(
            parsed,
            json!({"columns": ["a"], "rows": [["abcdef"]]}),
            "default execute_sql response must stay wire-compatible",
        );
    }

    #[test]
    fn chrome_tool_response_adds_metadata_before_rows_without_dropping_rows() {
        let table = decoded_table(
            &["id", "url"],
            vec![
                vec![json!(1), json!("https://example.test/a")],
                vec![json!(2), json!("https://example.test/b")],
            ],
        );

        let response = format_chrome_tool_response_with_redaction(
            table,
            2,
            DEFAULT_TOOL_MAX_STRING_LEN,
            false,
        )
        .expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_json_key_order(&response, "\"truncated\":", "\"rows\":");
        assert_eq!(parsed["columns"], json!(["id", "url"]));
        assert_eq!(
            parsed["rows"],
            json!([[1, "https://example.test/a"], [2, "https://example.test/b"]])
        );
        assert_eq!(parsed["row_count"], serde_json::Value::Null);
        assert_eq!(parsed["returned_rows"], json!(2));
        assert_eq!(parsed["truncated"], json!(true));
        assert_eq!(parsed["row_count_known"], json!(false));
        assert_eq!(parsed["redacted"], json!(false));
        assert!(
            parsed["note"]
                .as_str()
                .expect("note string")
                .contains("row_count unknown"),
            "note must explain Chrome tool completeness metadata: {parsed}",
        );
    }

    #[test]
    fn chrome_tool_response_uses_server_side_string_redaction() {
        let table = decoded_table(
            &["url"],
            vec![vec![json!(
                "https://px.effirst.com/api/v1/otelconfig?wpk-header=secret&ok=1"
            )]],
        );

        let response = format_chrome_tool_response_with_redaction(
            table,
            DEFAULT_CHROME_TOOL_ROWS,
            DEFAULT_TOOL_MAX_STRING_LEN,
            true,
        )
        .expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(
            parsed["rows"][0][0],
            json!("https://px.effirst.com/api/v1/otelconfig?wpk-header=<redacted>&ok=1")
        );
        assert_eq!(parsed["redacted"], json!(true));
        assert_eq!(parsed["string_truncated"], json!(false));
    }

    #[test]
    fn chrome_tool_response_preserves_long_strings_by_default() {
        let long = "abcdefghijklmnopqrstuvwxyz".repeat(12);
        let table = decoded_table(&["name"], vec![vec![json!(long.clone())]]);

        let response =
            format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, None).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        let returned = parsed["rows"][0][0].as_str().expect("string cell");
        assert_eq!(
            returned, long,
            "Chrome-tool strings should be precise by default"
        );
        assert_eq!(parsed["string_truncated"], json!(false));
    }

    #[test]
    fn chrome_tool_response_truncates_long_strings_when_requested() {
        let long = "abcdefghijklmnopqrstuvwxyz".repeat(12);
        let table = decoded_table(&["name"], vec![vec![json!(long)]]);

        let response = format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, Some(24))
            .expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        let returned = parsed["rows"][0][0].as_str().expect("string cell");
        assert!(
            returned.ends_with("...<truncated>"),
            "explicit max_string_len should cap long Chrome-tool strings: {returned}"
        );
        assert_eq!(
            returned.chars().count(),
            24 + "...<truncated>".chars().count()
        );
        assert_eq!(parsed["string_truncated"], json!(true));
    }

    #[test]
    fn chrome_tool_response_rejects_zero_max_string_len() {
        let table = decoded_table(&["name"], vec![vec![json!("short")]]);

        let err = format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, Some(0))
            .expect_err("zero max_string_len must reject");

        assert!(err.contains("max_string_len"), "got: {err}");
    }

    #[test]
    fn slice_descendants_response_echoes_summary_bounds_before_rows() {
        let table = decoded_table(
            &["root_id", "depth", "name", "slice_count", "total_ms"],
            vec![vec![
                json!(10),
                json!(1),
                json!("child"),
                json!(2),
                json!(3.5),
            ]],
        );
        let applied_filters = SliceDescendantsAppliedFilters {
            min_dur_ms: DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
            max_depth: DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
            limit: DEFAULT_SLICE_DESCENDANTS_LIMIT,
            include_args: false,
        };

        let response = format_slice_descendants_tool_response_with_redaction(
            table,
            DEFAULT_SLICE_DESCENDANTS_LIMIT as usize,
            applied_filters,
            Vec::new(),
            None,
            false,
        )
        .expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_json_key_order(&response, "\"applied_filters\":", "\"rows\":");
        assert_eq!(
            parsed["summary_scope"],
            json!(SLICE_DESCENDANTS_BREAKDOWN_SCOPE)
        );
        assert_eq!(
            parsed["applied_filters"],
            json!({
                "min_dur_ms": DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
                "max_depth": DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
                "limit": DEFAULT_SLICE_DESCENDANTS_LIMIT,
                "include_args": false,
            })
        );
        assert_eq!(
            parsed["missing_root_ids"],
            json!([] as [i64; 0]),
            "missing_root_ids must be present and empty when all roots existed: {parsed}",
        );
        assert!(
            parsed["note"]
                .as_str()
                .expect("note string")
                .contains("matching min_dur_ms within max_depth"),
            "note must explain bounded summary semantics: {parsed}",
        );
    }

    #[test]
    fn slice_descendants_response_includes_missing_root_ids() {
        let table = decoded_table(&["root_id", "depth", "name", "slice_count"], vec![]);
        let applied_filters = SliceDescendantsAppliedFilters {
            min_dur_ms: DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
            max_depth: DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
            limit: DEFAULT_SLICE_DESCENDANTS_LIMIT,
            include_args: false,
        };

        let response = format_slice_descendants_tool_response_with_redaction(
            table,
            DEFAULT_SLICE_DESCENDANTS_LIMIT as usize,
            applied_filters,
            vec![42, 99],
            None,
            false,
        )
        .expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(
            parsed["missing_root_ids"],
            json!([42, 99]),
            "missing_root_ids must echo back the stale ids in caller order: {parsed}",
        );
        assert_eq!(
            parsed["returned_rows"],
            json!(0),
            "empty descendants must still emit the envelope: {parsed}",
        );
        assert!(
            parsed["note"]
                .as_str()
                .expect("note string")
                .contains("missing_root_ids"),
            "shaping note must explain missing_root_ids semantics: {parsed}",
        );
    }

    #[test]
    fn slice_descendants_applied_filters_use_effective_defaults() {
        let params = SliceDescendantsBreakdownParams {
            slice_ids: vec![10],
            min_dur_ms: None,
            max_depth: None,
            include_args: true,
            limit: None,
            max_string_len: None,
        };

        let filters = slice_descendants_applied_filters(&params, DEFAULT_SLICE_DESCENDANTS_LIMIT);

        assert_eq!(
            filters,
            SliceDescendantsAppliedFilters {
                min_dur_ms: DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
                max_depth: DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
                limit: DEFAULT_SLICE_DESCENDANTS_LIMIT,
                include_args: true,
            }
        );
    }

    #[test]
    fn slice_descendants_error_hint_recommends_table_or_column_inspection() {
        let table_err = format_slice_descendants_tool_error(PerfettoError::QueryError {
            kind: QueryErrorKind::MissingTable,
            message: "no such table: slice".to_owned(),
        });
        assert!(
            table_err.contains("requires the base `slice` table")
                && table_err.contains("list_tables"),
            "missing-table hint must point at the schema check: {table_err}",
        );

        let column_err = format_slice_descendants_tool_error(PerfettoError::QueryError {
            kind: QueryErrorKind::MissingColumn,
            message: "no such column: slice.parent_id".to_owned(),
        });
        assert!(
            column_err.contains("list_table_structure") && column_err.contains("slice.id"),
            "missing-column hint must point at column inspection: {column_err}",
        );

        let other_err = format_slice_descendants_tool_error(PerfettoError::QueryError {
            kind: QueryErrorKind::Other,
            message: "syntax error near 'WITH'".to_owned(),
        });
        assert!(
            other_err.starts_with("Failed to run slice_descendants_breakdown:")
                && other_err.contains("syntax error"),
            "other errors must pass through with the tool prefix: {other_err}",
        );
    }

    #[test]
    fn execute_sql_head_limits_returned_rows_and_marks_truncation() {
        let table = decoded_table(&["n"], vec![vec![json!(1)], vec![json!(2)]]);
        let mut params = execute_sql_params("SELECT n FROM nums");
        params.head = Some(1);

        let response =
            format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(parsed["rows"], json!([[1]]));
        assert_json_key_order(&response, "\"truncated\":", "\"rows\":");
        assert_eq!(parsed["row_count"], json!(2));
        assert_eq!(parsed["returned_rows"], json!(1));
        assert_eq!(parsed["truncated"], json!(true));
        assert_eq!(parsed["row_count_known"], json!(true));
        assert!(
            parsed["note"]
                .as_str()
                .expect("note string")
                .contains("post-SQL decoded rows"),
            "note must prevent SQL-limit confusion: {parsed}",
        );
    }

    #[test]
    fn execute_sql_summary_uses_default_sample_rows() {
        let rows = (0..12).map(|n| vec![json!(n)]).collect();
        let table = decoded_table(&["n"], rows);
        let mut params = execute_sql_params("SELECT n FROM nums");
        params.summary = true;

        let response =
            format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_json_key_order(&response, "\"truncated\":", "\"sample_rows\":");
        assert_eq!(
            parsed["sample_rows"].as_array().expect("sample rows").len(),
            DEFAULT_EXECUTE_SQL_SUMMARY_ROWS,
        );
        assert_eq!(parsed["row_count"], json!(12));
        assert_eq!(parsed["returned_rows"], json!(10));
        assert_eq!(parsed["truncated"], json!(true));
        assert!(
            parsed.get("rows").is_none(),
            "summary must not emit full rows"
        );
    }

    #[test]
    fn execute_sql_columns_only_omits_rows() {
        let table = decoded_table(&["a", "b"], vec![vec![json!(1), json!("x")]]);
        let mut params = execute_sql_params("SELECT 1 AS a, 'x' AS b");
        params.columns_only = true;

        let response =
            format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(parsed["columns"], json!(["a", "b"]));
        assert_eq!(parsed["row_count"], json!(1));
        assert_eq!(parsed["returned_rows"], json!(0));
        assert!(parsed.get("rows").is_none(), "columns_only must omit rows");
        assert!(
            parsed.get("sample_rows").is_none(),
            "columns_only must omit sample_rows"
        );
    }

    #[test]
    fn execute_sql_max_string_len_truncates_returned_cells() {
        let table = decoded_table(&["s"], vec![vec![json!("abcdefghijklmnopqrstuvwxyz")]]);
        let mut params = execute_sql_params("SELECT long_string AS s");
        params.max_string_len = Some(8);

        let response =
            format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(parsed["rows"][0][0], json!("abcdefgh...<truncated>"));
        assert_eq!(parsed["string_truncated"], json!(true));
        assert_eq!(parsed["redacted"], json!(false));
    }

    #[test]
    fn execute_sql_redact_strings_masks_common_sensitive_values() {
        let table = decoded_table(
            &["header", "path", "url"],
            vec![vec![
                json!("Authorization: Bearer secret\r\nUser-Agent: test"),
                json!("C:\\Users\\FanTao\\AppData\\Local\\Qianwen"),
                json!("https://example.test/?access_token=secret-token&ok=1"),
            ]],
        );
        let params = execute_sql_params("SELECT sensitive_cells");

        let response =
            format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(
            parsed["rows"][0][0],
            json!("Authorization: <redacted>\r\nUser-Agent: test")
        );
        assert_eq!(
            parsed["rows"][0][1],
            json!("C:\\Users\\<user>\\AppData\\Local\\Qianwen")
        );
        assert_eq!(
            parsed["rows"][0][2],
            json!("https://example.test/?access_token=<redacted>&ok=1")
        );
        assert_eq!(parsed["redacted"], json!(true));
    }

    #[test]
    fn execute_sql_redact_strings_handles_token_at_eof() {
        let table = decoded_table(
            &["url"],
            vec![vec![json!(
                "https://example.test/?access_token=secret-token"
            )]],
        );
        let params = execute_sql_params("SELECT url");

        let response =
            format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(
            parsed["rows"][0][0],
            json!("https://example.test/?access_token=<redacted>")
        );
        assert_eq!(parsed["redacted"], json!(true));
    }

    #[test]
    fn execute_sql_redact_strings_masks_query_signatures_and_encoded_values() {
        let table = decoded_table(
            &["top_level", "encoded_nested"],
            vec![vec![
                json!(
                    "https://px.effirst.com/api/v1/jconfig?wpk-header=app%3Dueocxfzk%26ud%3Duser-42%26sign%3Dabc123&ok=1"
                ),
                json!("payload=app%3Ddemo%26sign%3Dabc123%26ud%3Duser-42%26safe%3Dkeep"),
            ]],
        );
        let params = execute_sql_params("SELECT url_args");

        let response =
            format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(
            parsed["rows"][0][0],
            json!("https://px.effirst.com/api/v1/jconfig?wpk-header=<redacted>&ok=1")
        );
        assert_eq!(
            parsed["rows"][0][1],
            json!("payload=app%3Ddemo%26sign%3D<redacted>%26ud%3D<redacted>%26safe%3Dkeep")
        );
        assert_eq!(parsed["redacted"], json!(true));
    }

    #[test]
    fn execute_sql_redact_strings_respects_query_key_boundaries() {
        let table = decoded_table(
            &["false_positive", "encoded_false_positive", "mixed"],
            vec![vec![
                json!("https://example.test/?design=dark&cloud=prod&guid=abc"),
                json!("prefixéésign%3Dabc"),
                json!("https://example.test/?design=dark&sign=real&cloud=prod&uid=user-42"),
            ]],
        );
        let params = execute_sql_params("SELECT url_args");

        let response =
            format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(
            parsed["rows"][0][0],
            json!("https://example.test/?design=dark&cloud=prod&guid=abc")
        );
        assert_eq!(parsed["rows"][0][1], json!("prefixéésign%3Dabc"));
        assert_eq!(
            parsed["rows"][0][2],
            json!("https://example.test/?design=dark&sign=<redacted>&cloud=prod&uid=<redacted>")
        );
        assert_eq!(parsed["redacted"], json!(true));
    }

    #[test]
    fn execute_sql_redact_strings_masks_terminal_user_profile_paths() {
        let table = decoded_table(
            &["win", "win_slash", "mac", "linux"],
            vec![vec![
                json!("C:\\Users\\Alice"),
                json!("C:/Users/Alice"),
                json!("/Users/Alice"),
                json!("/home/alice"),
            ]],
        );
        let params = execute_sql_params("SELECT profile_paths");

        let response =
            format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

        assert_eq!(parsed["rows"][0][0], json!("C:\\Users\\<user>"));
        assert_eq!(parsed["rows"][0][1], json!("C:/Users/<user>"));
        assert_eq!(parsed["rows"][0][2], json!("/Users/<user>"));
        assert_eq!(parsed["rows"][0][3], json!("/home/<user>"));
        assert_eq!(parsed["redacted"], json!(true));
    }

    #[test]
    fn execute_sql_output_shape_rejects_conflicting_or_zero_limits() {
        let mut params = execute_sql_params("SELECT 1");
        params.head = Some(1);
        params.limit = Some(1);
        let err = execute_sql_output_shape(&params, false).expect_err("head + limit must reject");
        assert!(err.contains("head"), "got: {err}");

        let mut params = execute_sql_params("SELECT 1");
        params.limit = Some(0);
        let err = execute_sql_output_shape(&params, false).expect_err("limit=0 must reject");
        assert!(err.contains("> 0"), "got: {err}");

        let mut params = execute_sql_params("SELECT 1");
        params.max_string_len = Some(0);
        let err =
            execute_sql_output_shape(&params, false).expect_err("max_string_len=0 must reject");
        assert!(err.contains("max_string_len"), "got: {err}");
    }

    #[test]
    fn execute_sql_params_accept_stringified_numeric_shaping_fields() {
        let p: ExecuteSqlParams =
            serde_json::from_str(r#"{"sql": "SELECT 1", "head": "3", "max_string_len": "40"}"#)
                .expect("stringified numeric shaping fields must deserialize");
        assert_eq!(p.head, Some(3));
        assert_eq!(p.max_string_len, Some(40));
    }

    #[test]
    fn execute_sql_rejects_redaction_as_tool_parameter() {
        let err = serde_json::from_str::<ExecuteSqlParams>(
            r#"{"sql": "SELECT 1", "redact_strings": true}"#,
        )
        .expect_err("redaction must be controlled by server policy, not the LLM");
        assert!(
            err.to_string().contains("redact_strings"),
            "error must name the rejected field, got: {err}",
        );
    }

    // The description is a proc-macro string literal so it can't interpolate
    // MAX_ROWS. Pin the literal against the constant so changing MAX_ROWS
    // without updating the description fails here instead of misleading agents.
    #[test]
    fn execute_sql_description_matches_max_rows_constant() {
        let server = test_server();
        let tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == "execute_sql")
            .expect("execute_sql tool must exist");
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(
            desc.contains(&MAX_ROWS.to_string()),
            "execute_sql description must mention MAX_ROWS ({MAX_ROWS}), got: {desc}",
        );
    }

    fn test_server() -> PerfettoMcpServer {
        let manager = Arc::new(TraceProcessorManager::new_with_binary(
            PathBuf::from("/nonexistent/trace_processor_shell"),
            1,
        ));
        PerfettoMcpServer::new(manager)
    }

    // Without these capabilities, clients skip `tools/list` / `resources/list`
    // on handshake and the router still has them, but they're invisible.
    #[test]
    fn get_info_declares_tools_and_resources_capabilities() {
        let info = test_server().get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "server must declare `tools` capability so clients call tools/list"
        );
        assert!(
            info.capabilities.resources.is_some(),
            "server must declare `resources` capability so clients can read quickrefs"
        );
    }

    #[test]
    fn instructions_stay_short_and_route_to_quickref() {
        let info = test_server().get_info();
        let instructions = info
            .instructions
            .expect("server must ship short routing instructions");
        assert!(
            instructions.len() <= 900,
            "instructions should stay routing-sized; got {} chars: {instructions}",
            instructions.len(),
        );
        assert!(
            instructions.contains(STDLIB_QUICKREF_URI),
            "instructions must route long stdlib guidance to the quickref resource"
        );
        assert!(
            instructions.contains("list_stdlib_modules"),
            "instructions must preserve a tools-only fallback for stdlib discovery"
        );
    }

    #[test]
    fn stdlib_quickref_resource_metadata_is_exposed() {
        let resource = stdlib_quickref_resource();
        assert_eq!(resource.uri, STDLIB_QUICKREF_URI);
        assert_eq!(
            resource.mime_type.as_deref(),
            Some(STDLIB_QUICKREF_MIME_TYPE)
        );
        assert!(
            resource.size.unwrap_or_default() as usize >= STDLIB_QUICKREF.len(),
            "resource size should describe the quickref payload"
        );
    }

    #[test]
    fn instructions_surface_server_redaction_policy() {
        let enabled = server_instructions_for_redaction(true);
        assert!(
            enabled.contains("SQL/Chrome tool string redaction is enabled"),
            "instructions must surface enabled privacy policy: {enabled}",
        );
        assert!(
            enabled.contains(REDACT_STRINGS_DEFAULT_ENV),
            "instructions must tell users how to control the policy: {enabled}",
        );

        let disabled = server_instructions_for_redaction(false);
        assert!(
            disabled.contains("SQL/Chrome tool string redaction is disabled"),
            "instructions must surface disabled privacy policy: {disabled}",
        );
    }

    #[test]
    fn tool_router_exposes_expected_tools() {
        let server = test_server();
        let mut names: Vec<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "chrome_main_thread_hotspots",
                "chrome_page_load_summary",
                "chrome_scroll_jank_summary",
                "chrome_startup_summary",
                "chrome_web_content_interactions",
                "execute_sql",
                "list_processes",
                "list_stdlib_modules",
                "list_table_structure",
                "list_tables",
                "list_threads_in_process",
                "load_trace",
                "slice_descendants_breakdown",
            ],
        );
    }

    #[test]
    fn tool_annotations_surface_safety_hints() {
        let server = test_server();
        for tool in server.tool_router.list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("tool `{}` must carry annotations", tool.name));
            assert_eq!(
                annotations.open_world_hint,
                Some(false),
                "tool `{}` should advertise a closed-world trace-analysis domain",
                tool.name,
            );
            assert_eq!(
                annotations.destructive_hint,
                Some(false),
                "tool `{}` should advertise non-destructive behavior",
                tool.name,
            );
            assert_eq!(
                annotations.idempotent_hint,
                Some(true),
                "tool `{}` should advertise idempotence for repeated calls",
                tool.name,
            );
            if tool.name == "load_trace" {
                assert_eq!(
                    annotations.read_only_hint, None,
                    "load_trace changes server current-trace state, so it should not claim read-only"
                );
            } else {
                assert_eq!(
                    annotations.read_only_hint,
                    Some(true),
                    "tool `{}` should advertise read-only behavior",
                    tool.name,
                );
            }
        }
    }

    #[test]
    fn tool_descriptions_stay_within_context_budget() {
        let server = test_server();
        let tools = server.tool_router.list_all();
        let total: usize = tools
            .iter()
            .map(|tool| tool.description.as_deref().unwrap_or("").len())
            .sum();
        assert!(
            total <= 15_000,
            "tool descriptions should stay routing-sized; total={total}"
        );
        for tool in tools {
            let len = tool.description.as_deref().unwrap_or("").len();
            assert!(
                len <= 2_500,
                "tool `{}` description is too large for default tools/list context: {len}",
                tool.name,
            );
        }
    }

    #[test]
    fn chrome_tool_hint_fires_on_missing_table() {
        let formatted = format_chrome_tool_error(
            "Chrome scroll jank summary",
            PerfettoError::QueryError {
                kind: QueryErrorKind::MissingTable,
                message: "no such table: chrome_janky_frames".to_owned(),
            },
        );
        assert!(
            formatted.contains("stdlib view"),
            "missing-table hint must describe the stdlib-view-drift case, got: {formatted}",
        );
        assert!(
            formatted.contains("list_tables"),
            "hint must point at list_tables for schema discovery, got: {formatted}",
        );
        assert!(
            !formatted.contains("requires a Chrome trace"),
            "missing-table must NOT blame trace type — preflight already rules that out, got: {formatted}",
        );
    }

    #[test]
    fn chrome_tool_hint_fires_on_missing_module() {
        let formatted = format_chrome_tool_error(
            "Chrome page load summary",
            PerfettoError::QueryError {
                kind: QueryErrorKind::MissingModule,
                message: "Module not found: chrome.page_loads".to_owned(),
            },
        );
        assert!(
            formatted.contains("stdlib module"),
            "missing-module errors must surface the stdlib-binary hint, got: {formatted}",
        );
        assert!(
            formatted.contains("PERFETTO_TP_PATH"),
            "missing-module hint must mention PERFETTO_TP_PATH as the binary override, got: {formatted}",
        );
        assert!(
            !formatted.contains("Chrome trace"),
            "missing-module must NOT misdiagnose as 'not a Chrome trace', got: {formatted}",
        );
    }

    #[test]
    fn chrome_tool_skips_unrelated_query_errors() {
        let formatted = format_chrome_tool_error(
            "Chrome main-thread hotspots",
            PerfettoError::QueryError {
                kind: QueryErrorKind::Other,
                message: "syntax error near GROUP".to_owned(),
            },
        );
        assert!(
            !formatted.contains("Chrome trace"),
            "unrelated SQL errors must not get the Chrome-trace hint, got: {formatted}",
        );
        assert!(
            formatted.contains("syntax error"),
            "unrelated errors must still surface the original message, got: {formatted}",
        );
    }

    #[test]
    fn list_stdlib_modules_returns_curated_set() {
        let json: serde_json::Value = serde_json::from_str(STDLIB_MODULE_LIST)
            .expect("STDLIB_MODULE_LIST must be valid JSON");
        let modules = json.as_array().expect("must be a JSON array");

        assert_eq!(
            modules.len(),
            10,
            "STDLIB_MODULE_LIST must contain exactly 10 modules, got {}",
            modules.len()
        );

        let module_names: Vec<&str> = modules
            .iter()
            .map(|m| m["module"].as_str().expect("module field must be a string"))
            .collect();

        for expected in [
            "chrome.page_loads",
            "chrome.scroll_jank.scroll_jank_v3",
            "chrome.tasks",
            "chrome.startups",
            "chrome.web_content_interactions",
            "android.startup.startups",
            "android.anrs",
            "android.binder",
            "slices.with_context",
            "linux.cpu.frequency",
        ] {
            assert!(
                module_names.contains(&expected),
                "STDLIB_MODULE_LIST missing module `{expected}`",
            );
        }

        for module in modules {
            let name = module["module"].as_str().unwrap();
            assert!(
                module["views"].as_array().is_some()
                    && !module["views"].as_array().unwrap().is_empty(),
                "module `{name}` must have non-empty views array",
            );
            assert!(
                module["description"].as_str().is_some(),
                "module `{name}` must have description",
            );
            assert!(
                module["usage"].as_str().is_some(),
                "module `{name}` must have usage example",
            );
        }
    }

    #[test]
    fn list_stdlib_modules_filters_by_domain_query_and_limit() {
        let response = filtered_stdlib_modules_json(&ListStdlibModulesParams {
            domain: Some("chrome".to_owned()),
            query: Some("jank".to_owned()),
            limit: Some(2),
        })
        .expect("filters should serialize");
        let modules: Vec<serde_json::Value> = serde_json::from_str(&response).unwrap();

        assert_eq!(modules.len(), 1);
        assert_eq!(
            modules[0]["module"].as_str(),
            Some("chrome.scroll_jank.scroll_jank_v3")
        );
    }

    #[test]
    fn list_stdlib_modules_rejects_invalid_filter_values() {
        let err = filtered_stdlib_modules_json(&ListStdlibModulesParams {
            domain: Some("ios".to_owned()),
            query: None,
            limit: None,
        })
        .expect_err("unknown domains must be rejected");
        assert!(err.contains("domain"), "got: {err}");

        let err = filtered_stdlib_modules_json(&ListStdlibModulesParams {
            domain: None,
            query: None,
            limit: Some(0),
        })
        .expect_err("zero limit must be rejected");
        assert!(err.contains("limit"), "got: {err}");
    }

    /// Integration test exercising the full wrapper path of EVERY chrome_*
    /// handler: client_for → ensure_chrome_trace → error-on-non-chrome.
    /// Guards against regressions where any future handler forgets to call
    /// the preflight (SQL-level e2e wouldn't catch that). Five calls share
    /// one tp_shell spawn because the manager caches clients by path.
    #[test]
    fn all_chrome_handlers_reject_non_chrome_via_preflight() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        runtime.block_on(async {
            let manager = Arc::new(TraceProcessorManager::new_with_starting_port(1, 19_021));
            let server = PerfettoMcpServer::new(manager);
            let non_chrome_path = "tests/fixtures/basic.perfetto-trace";
            // `load_trace` first so subsequent handlers see a valid current
            // trace (preflight rejection is then about chrome-vs-non-chrome,
            // not about "no trace loaded").
            server
                .load_trace(Parameters(LoadTraceParams {
                    path: non_chrome_path.to_owned(),
                }))
                .await
                .expect("load_trace on non-chrome fixture must succeed");

            let r = server
                .chrome_scroll_jank_summary(Parameters(ChromeTraceParams {
                    max_string_len: None,
                }))
                .await;
            let err = r
                .map(|_| ())
                .expect_err("chrome_scroll_jank_summary: preflight must reject");
            assert!(err.contains("Chrome scroll jank summary"), "got: {err}");
            assert!(err.contains("Chrome-family trace"), "got: {err}");
            assert!(err.contains("list_stdlib_modules"), "got: {err}");

            let r = server
                .chrome_page_load_summary(Parameters(ChromeTraceParams {
                    max_string_len: None,
                }))
                .await;
            let err = r
                .map(|_| ())
                .expect_err("chrome_page_load_summary: preflight must reject");
            assert!(err.contains("Chrome page load summary"), "got: {err}");
            assert!(err.contains("Chrome-family trace"), "got: {err}");
            assert!(err.contains("list_stdlib_modules"), "got: {err}");

            let r = server
                .chrome_main_thread_hotspots(Parameters(ChromeMainThreadHotspotsParams {
                    process_name: None,
                    pid: None,
                    upid: None,
                    page_load_id: None,
                    navigation_id: None,
                    phase: None,
                    start_ts_ns: None,
                    end_ts_ns: None,
                    min_dur_ms: None,
                    limit: None,
                    max_string_len: None,
                }))
                .await;
            let err = r
                .map(|_| ())
                .expect_err("chrome_main_thread_hotspots: preflight must reject");
            assert!(err.contains("Chrome main-thread hotspots"), "got: {err}");
            assert!(err.contains("Chrome-family trace"), "got: {err}");
            assert!(err.contains("list_stdlib_modules"), "got: {err}");

            let r = server
                .chrome_startup_summary(Parameters(ChromeTraceParams {
                    max_string_len: None,
                }))
                .await;
            let err = r
                .map(|_| ())
                .expect_err("chrome_startup_summary: preflight must reject");
            assert!(err.contains("Chrome startup summary"), "got: {err}");
            assert!(err.contains("Chrome-family trace"), "got: {err}");
            assert!(err.contains("list_stdlib_modules"), "got: {err}");

            let r = server
                .chrome_web_content_interactions(Parameters(ChromeTraceParams {
                    max_string_len: None,
                }))
                .await;
            let err = r
                .map(|_| ())
                .expect_err("chrome_web_content_interactions: preflight must reject");
            assert!(
                err.contains("Chrome web content interactions"),
                "got: {err}"
            );
            assert!(err.contains("Chrome-family trace"), "got: {err}");
            assert!(err.contains("list_stdlib_modules"), "got: {err}");
        });
    }

    /// Regression net: the format parameter and SqlResultFormat enum were
    /// removed; description must not silently drift back in.
    #[test]
    fn execute_sql_description_does_not_mention_format_param() {
        let server = test_server();
        let tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == "execute_sql")
            .expect("execute_sql tool must exist");
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(
            !desc.contains("format"),
            "execute_sql description must not mention `format` parameter, got: {desc}",
        );
    }

    /// Pin the description trim — `outputSchema` carries the shape now,
    /// so the literal columnar layout sample must NOT appear in prose.
    #[test]
    fn execute_sql_description_does_not_spell_out_columnar_shape() {
        let server = test_server();
        let tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == "execute_sql")
            .expect("execute_sql tool must exist");
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(
            !desc.contains("{columns:"),
            "execute_sql description must not spell out the columnar shape, got: {desc}",
        );
    }

    #[test]
    fn execute_sql_schema_does_not_expose_redaction_control() {
        let server = test_server();
        let tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == "execute_sql")
            .expect("execute_sql tool must exist");
        let schema =
            serde_json::to_string(&tool.input_schema).expect("input schema must serialize");
        assert!(
            !schema.contains("redact_strings"),
            "redaction is server-side policy and must not be exposed to the LLM: {schema}",
        );
    }

    /// Pin the schema-discovery tool descriptions' "do NOT use in execute_sql"
    /// disclaimer. Motivated by a v0.11.2 session log showing the LLM querying
    /// `SELECT * FROM list_table_structure WHERE 0` (a wasted execute_sql call
    /// that errored, after which the LLM correctly invoked the tool directly).
    /// Both `list_tables` and `list_table_structure` carry the same nudge so
    /// the LLM sees it on whichever schema-discovery surface it reaches first.
    #[test]
    fn schema_discovery_tools_warn_against_execute_sql_misuse() {
        let server = test_server();
        for tool_name in ["list_tables", "list_table_structure"] {
            let tool = server
                .tool_router
                .list_all()
                .into_iter()
                .find(|t| t.name == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} tool must exist"));
            let desc = tool.description.as_deref().unwrap_or("");
            assert!(
                desc.contains("execute_sql"),
                "{tool_name} description must explicitly mention execute_sql to \
                 anchor the disclaimer, got: {desc}",
            );
            assert!(
                desc.contains("separate MCP tool"),
                "{tool_name} description must say it is a separate MCP tool to \
                 prevent the LLM from treating it as a virtual table, got: {desc}",
            );
        }
    }

    // -- v0.11.3 lenient numeric deserializer tests ----------------------
    //
    // Numeric tool params accept both JSON numbers and JSON strings holding
    // the same value. Motivated by a v0.11.2 Claude Code session that
    // stringified every numeric argument and bounced 4 times before giving
    // up entirely. The schema still advertises `integer`/`number` so
    // well-behaved LLMs see strict types.

    /// End-to-end on the actual params type: the v0.11.2 session's failing
    /// JSON `{pid: "12800", min_dur_ms: "50", limit: "30"}` now deserializes
    /// successfully into the typed params.
    #[test]
    fn chrome_main_thread_hotspots_params_accepts_stringified_numerics() {
        let p: ChromeMainThreadHotspotsParams = serde_json::from_str(
            r#"{"pid": "12800", "page_load_id": "1", "start_ts_ns": "100", "end_ts_ns": "200", "min_dur_ms": "50", "limit": "30", "max_string_len": "260"}"#,
        )
        .expect("stringified numerics must deserialize after v0.11.3");
        assert_eq!(p.pid, Some(12800));
        assert_eq!(p.page_load_id, Some(1));
        assert_eq!(p.start_ts_ns, Some(100));
        assert_eq!(p.end_ts_ns, Some(200));
        assert_eq!(p.min_dur_ms, Some(50.0));
        assert_eq!(p.limit, Some(30));
        assert_eq!(p.max_string_len, Some(260));
    }

    #[test]
    fn chrome_main_thread_hotspots_params_accept_navigation_id() {
        let p: ChromeMainThreadHotspotsParams =
            serde_json::from_str(r#"{"navigation_id": "7", "phase": "dcl_to_fcp"}"#)
                .expect("navigation_id and phase must deserialize");
        assert_eq!(p.page_load_id, None);
        assert_eq!(p.navigation_id, Some(7));
        assert_eq!(p.phase, Some(ChromeMainThreadHotspotsPhase::DclToFcp));
    }

    #[test]
    fn chrome_trace_params_accept_stringified_max_string_len() {
        let p: ChromeTraceParams = serde_json::from_str(r#"{"max_string_len": "300"}"#)
            .expect("stringified Chrome max_string_len must deserialize");
        assert_eq!(p.max_string_len, Some(300));
    }

    /// JsonSchema must still advertise strict types so well-behaved LLMs
    /// don't see "string-or-integer" weirdness on `tools/list`. The
    /// `deserialize_with` is server-side leniency only, invisible to the
    /// schema. Pin this against the actual `tools/list` payload for
    /// `chrome_main_thread_hotspots`.
    #[test]
    fn schema_for_chrome_hotspots_advertises_strict_numeric_types() {
        let server = test_server();
        let tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == "chrome_main_thread_hotspots")
            .expect("tool must exist");
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("input schema must have a `properties` object");
        // Each numeric field must advertise its simple type — never a union
        // with "string", never an `anyOf`. The lenient deserializer accepts
        // strings server-side; the schema is for advertising strict types
        // to well-behaved LLMs.
        let strict_pairs: &[(&str, &str)] = &[
            ("pid", "integer"),
            ("upid", "integer"),
            ("page_load_id", "integer"),
            ("navigation_id", "integer"),
            ("start_ts_ns", "integer"),
            ("end_ts_ns", "integer"),
            ("min_dur_ms", "number"),
            ("limit", "integer"),
            ("max_string_len", "integer"),
        ];
        for (field, expected_type) in strict_pairs {
            let prop = props
                .get(*field)
                .unwrap_or_else(|| panic!("`{field}` field missing from schema"));
            // The field is `Option<T>`, so the schema is either
            // `{"type": ["<expected_type>", "null"], ...}` (with null
            // explicit) or carries the type via a single string. Both shapes
            // must NOT include "string", and must NOT use anyOf.
            assert!(
                prop.get("anyOf").is_none(),
                "`{field}` schema must not use anyOf: {prop}",
            );
            let ty = prop
                .get("type")
                .unwrap_or_else(|| panic!("`{field}` schema missing `type`: {prop}"));
            let advertises_string = match ty {
                serde_json::Value::String(s) => s == "string",
                serde_json::Value::Array(arr) => arr.iter().any(|v| v.as_str() == Some("string")),
                _ => false,
            };
            assert!(
                !advertises_string,
                "`{field}` schema must not advertise string variant: {prop}",
            );
            // Sanity-check that the strict type IS present (not just
            // missing string).
            let advertises_expected = match ty {
                serde_json::Value::String(s) => s == *expected_type,
                serde_json::Value::Array(arr) => {
                    arr.iter().any(|v| v.as_str() == Some(*expected_type))
                }
                _ => false,
            };
            assert!(
                advertises_expected,
                "`{field}` schema must advertise `{expected_type}`: {prop}",
            );
        }
    }

    // -- v0.11.3 `name` alias on table_name ------------------------------

    #[test]
    fn list_table_structure_accepts_name_alias() {
        let from_canonical: TableStructureParams =
            serde_json::from_str(r#"{"table_name": "slice"}"#)
                .expect("canonical `table_name` must deserialize");
        let from_alias: TableStructureParams =
            serde_json::from_str(r#"{"name": "slice"}"#).expect("alias `name` must deserialize");
        assert_eq!(from_canonical.table_name, "slice");
        assert_eq!(from_alias.table_name, "slice");
    }

    // -- v0.11.3 current_trace state -------------------------------------

    /// With nothing loaded, `current_trace_path` returns a clear actionable
    /// error pointing the caller at `load_trace`. Every non-`load_trace`
    /// handler funnels through this, so all of them get the nudge.
    #[test]
    fn current_trace_path_errors_clearly_when_no_trace_loaded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let server = test_server();
            let err = server.current_trace_path().await.unwrap_err();
            assert!(
                err.contains("load_trace"),
                "error must reference load_trace, got: {err}",
            );
        });
    }

    /// The schema must NOT expose a `path` field on any non-`load_trace`
    /// tool — that's the central v0.11.3 contract. If anyone re-introduces
    /// `path` on, say, `execute_sql`, this test catches it on `tools/list`.
    #[test]
    fn only_load_trace_advertises_path_field() {
        let server = test_server();
        for tool in server.tool_router.list_all() {
            let schema = serde_json::to_value(&tool.input_schema).unwrap();
            let props = schema.get("properties").and_then(|p| p.as_object());
            let has_path = props.map(|p| p.contains_key("path")).unwrap_or(false);
            if tool.name == "load_trace" {
                assert!(has_path, "load_trace must advertise `path`");
            } else {
                assert!(
                    !has_path,
                    "tool `{}` must not advertise `path` (only load_trace does after v0.11.3)",
                    tool.name,
                );
            }
        }
    }

    /// v0.10.0 reverted `Json<T>` returns to plain `Result<String, String>`
    /// (Claude Code rendered `structured_content` as multi-line pretty-print,
    /// blowing up the conversation UI). With no tool returning `Json<T>`,
    /// none should carry an `outputSchema` — pin that absence so a future
    /// re-introduction of `Json<T>` is a deliberate, visible change.
    #[test]
    fn no_tool_carries_output_schema() {
        let server = test_server();
        for tool in server.tool_router.list_all() {
            assert!(
                tool.output_schema.is_none(),
                "tool {} must not carry an outputSchema (v0.10.0 contract)",
                tool.name,
            );
        }
    }

    /// v0.11.0 renamed the trace-file param from `trace_path` to `path`.
    /// v0.11.3 then removed `path` from every tool except `load_trace` (the
    /// remaining tools now read the current trace set by `load_trace`). So
    /// `load_trace` is the only entry point that needs to honor the legacy
    /// `trace_path` alias for v0.10.x callers. Pinned here.
    #[test]
    fn load_trace_accepts_trace_path_alias_for_backwards_compat() {
        let from_path: LoadTraceParams =
            serde_json::from_str(r#"{"path": "/x"}"#).expect("canonical `path` must deserialize");
        let from_alias: LoadTraceParams = serde_json::from_str(r#"{"trace_path": "/x"}"#)
            .expect("legacy `trace_path` alias must still deserialize");
        assert_eq!(from_path.path, "/x");
        assert_eq!(from_alias.path, "/x");
    }

    /// v0.11.3 removed `path` from non-`load_trace` tools. v0.10.x callers
    /// still passing `{path: "..."}` to `execute_sql` must now get a clear
    /// "unknown field path" error so the caller learns to drop it. This test
    /// also pins that `trace_path` (the v0.10.x alias) is rejected for the
    /// same reason — `deny_unknown_fields` no longer recognizes either.
    #[test]
    fn execute_sql_rejects_v0_10_x_path_field() {
        let r = serde_json::from_str::<ExecuteSqlParams>(r#"{"path": "/x", "sql": "SELECT 1"}"#);
        assert!(r.is_err(), "v0.10.x `path` field must now error, got Ok");
        let r =
            serde_json::from_str::<ExecuteSqlParams>(r#"{"trace_path": "/x", "sql": "SELECT 1"}"#);
        assert!(
            r.is_err(),
            "v0.10.x `trace_path` field must now error, got Ok",
        );
    }

    /// `#[serde(deny_unknown_fields)]` makes hallucinated fields fail fast
    /// instead of being silently dropped. Pinned on
    /// `ChromeMainThreadHotspotsParams` because that struct was the
    /// motivating incident — a v0.11.0 session passed
    /// `min_dur_xxxxxxx: "16"` (typo in the new field name) and got back a
    /// success with the default 16 ms threshold. With deny_unknown_fields,
    /// the same call now errors with the offending field named.
    #[test]
    fn chrome_main_thread_hotspots_params_rejects_unknown_fields() {
        let err = serde_json::from_str::<ChromeMainThreadHotspotsParams>(
            r#"{"threshold_ms": 16, "max_results": 25}"#,
        )
        .expect_err("unknown fields must produce an error");
        let msg = err.to_string();
        assert!(
            msg.contains("threshold_ms") || msg.contains("max_results"),
            "error must name at least one of the offending fields, got: {msg}",
        );
    }

    /// Same guarantee on `LoadTraceParams` — picks up future regressions if
    /// `deny_unknown_fields` is dropped from the most-called tool first.
    #[test]
    fn load_trace_params_rejects_unknown_fields() {
        let err = serde_json::from_str::<LoadTraceParams>(r#"{"path": "/x", "lazy": true}"#)
            .expect_err("unknown field `lazy` must error");
        assert!(
            err.to_string().contains("lazy"),
            "error must name the offending field, got: {err}",
        );
    }

    /// The advertised inputSchema reflects the closed contract too: schemars
    /// emits `additionalProperties: false` when `deny_unknown_fields` is set.
    /// LLMs reading `tools/list` see a closed schema and (in theory) are less
    /// prone to hallucinate fields. The advertised tools all carry params with
    /// `deny_unknown_fields`; rmcp's parameterless `async fn foo(&self)` shape,
    /// by contrast, emits an *open* schema and silently ignores hallucinated fields.
    /// If anyone drops the attribute, this test fails on the affected
    /// tool's schema.
    #[test]
    fn tool_input_schemas_advertise_closed_object() {
        let server = test_server();
        for tool in server.tool_router.list_all() {
            let schema_value =
                serde_json::to_value(&tool.input_schema).expect("input schema must serialize");
            let additional = schema_value.get("additionalProperties");
            assert_eq!(
                additional,
                Some(&json!(false)),
                "tool `{}` input schema must set additionalProperties=false \
                 (i.e. carry #[serde(deny_unknown_fields)] on its params), got: {schema_value}",
                tool.name,
            );
        }
    }

    /// Pin that `tools/list` advertises only the canonical `path` field.
    /// `trace_path` is a serde-only deserialization alias — it must NOT
    /// appear in the JSON Schema. If schemars ever started emitting
    /// aliases (or if someone reverted the rename), this fails.
    #[test]
    fn tool_input_schemas_use_path_not_trace_path() {
        let server = test_server();
        for tool in server.tool_router.list_all() {
            let schema_str =
                serde_json::to_string(&tool.input_schema).expect("input schema must serialize");
            assert!(
                !schema_str.contains("trace_path"),
                "tool {} input schema must advertise canonical `path` only, not \
                 the legacy `trace_path` alias; got: {schema_str}",
                tool.name,
            );
        }
    }

    /// No-filter SQL keeps the same `LEFT JOIN process p ON ct.upid = p.upid` clause
    /// as the filtered variants, so the join is harmless when no pid filter is
    /// set — this means handlers can always use the same builder.
    #[test]
    fn chrome_main_thread_hotspots_sql_no_filter_runs_all_main_threads() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters::default())
            .expect("builder must succeed");
        assert!(sql.contains("ct.ts"));
        assert!(sql.contains("ct.upid"));
        assert!(sql.contains("p.pid"));
        assert!(sql.contains("LEFT JOIN thread t ON ct.utid = t.utid"));
        assert!(sql.contains("LEFT JOIN process p ON ct.upid = p.upid"));
        assert!(sql.contains("WHERE (t.is_main_thread = 1 OR ct.thread_name GLOB 'Cr*Main')"));
        assert!(sql.contains("AND ct.dur > 16000000"));
        assert!(sql.contains("ORDER BY ct.dur DESC LIMIT 100"));
        assert!(
            !sql.contains("chrome.page_loads"),
            "no-filter SQL must not include page-load window CTE, got: {sql}",
        );
        assert!(
            !sql.contains("ct.process_name ="),
            "no-filter SQL must not emit process_name filter, got: {sql}",
        );
        assert!(
            !sql.contains("p.pid ="),
            "no-filter SQL must not emit pid filter, got: {sql}",
        );
        assert!(
            !sql.contains("p.upid ="),
            "no-filter SQL must not emit upid filter, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_uses_name_fallback_for_chromium_traces() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters::default())
            .expect("builder must succeed");
        assert!(
            sql.contains("ct.thread_name GLOB 'Cr*Main'"),
            "main-thread detection must fall back to Chrome thread names, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_with_pid_emits_filter() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            pid: Some(12800),
            ..Default::default()
        })
        .expect("pid-filter builder must succeed");
        assert!(sql.contains("AND p.pid = 12800"), "got: {sql}");
        assert!(
            !sql.contains("ct.process_name ="),
            "pid-only filter must not emit process_name clause, got: {sql}",
        );
        assert!(
            !sql.contains("p.upid ="),
            "pid-only filter must not emit upid clause, got: {sql}",
        );
    }

    /// upid is the trace-internal unique pid — precise even when the OS
    /// recycles a pid. Adds `AND p.upid = ?` to the WHERE clause.
    #[test]
    fn chrome_main_thread_hotspots_sql_with_upid_emits_filter() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            upid: Some(3),
            ..Default::default()
        })
        .expect("upid-filter builder must succeed");
        assert!(sql.contains("AND p.upid = 3"), "got: {sql}");
        assert!(
            !sql.contains("p.pid ="),
            "upid-only filter must not emit pid clause, got: {sql}",
        );
        assert!(
            !sql.contains("ct.process_name ="),
            "upid-only filter must not emit process_name clause, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_with_process_name_emits_filter() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            process_name: Some("Renderer"),
            ..Default::default()
        })
        .expect("name-filter builder must succeed");
        assert!(
            sql.contains("AND ct.process_name = 'Renderer'"),
            "process_name filter must use sql_string_literal quoting, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_with_both_filters_ands_them() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            process_name: Some("Renderer"),
            pid: Some(12800),
            ..Default::default()
        })
        .expect("combined-filter builder must succeed");
        assert!(
            sql.contains("AND ct.process_name = 'Renderer'"),
            "got: {sql}"
        );
        assert!(sql.contains("AND p.pid = 12800"), "got: {sql}");
    }

    /// Redundant `upid + pid` pairing is documented as harmless — both clauses
    /// emit and AND together. Useful when the LLM has both IDs handy from
    /// list_processes and wants a belt-and-suspenders filter.
    #[test]
    fn chrome_main_thread_hotspots_sql_with_upid_and_pid_emits_both() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            pid: Some(12800),
            upid: Some(3),
            ..Default::default()
        })
        .expect("upid+pid combined builder must succeed");
        assert!(sql.contains("AND p.pid = 12800"), "got: {sql}");
        assert!(sql.contains("AND p.upid = 3"), "got: {sql}");
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_with_raw_window_emits_ts_filters() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            start_ts_ns: Some(1000),
            end_ts_ns: Some(2000),
            ..Default::default()
        })
        .expect("raw-window builder must succeed");
        assert!(sql.contains("AND ct.ts >= 1000"), "got: {sql}");
        assert!(sql.contains("AND ct.ts < 2000"), "got: {sql}");
        assert!(
            !sql.contains("hotspot_window"),
            "raw timestamp filters alone must not require page_loads, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_with_page_load_defaults_to_nav_to_fcp() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            page_load_id: Some(7),
            ..Default::default()
        })
        .expect("page-load-window builder must succeed");
        assert!(
            sql.contains("INCLUDE PERFETTO MODULE chrome.page_loads;"),
            "got: {sql}"
        );
        assert!(
            sql.contains("SELECT navigation_start_ts AS start_ts, fcp_ts AS end_ts"),
            "page_load_id without phase must default to navigation_to_fcp, got: {sql}",
        );
        assert!(
            sql.contains("WHERE id = 7 "),
            "page_load_id must match only chrome_page_loads.id, got: {sql}",
        );
        assert!(
            !sql.contains("navigation_id = 7"),
            "page_load_id must not also match navigation_id, got: {sql}",
        );
        assert!(sql.contains("CROSS JOIN hotspot_window hw"), "got: {sql}");
        assert!(sql.contains("AND ct.ts >= hw.start_ts"), "got: {sql}");
        assert!(sql.contains("AND ct.ts < hw.end_ts"), "got: {sql}");
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_with_navigation_id_matches_navigation_only() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            navigation_id: Some(7),
            ..Default::default()
        })
        .expect("navigation-window builder must succeed");
        assert!(
            sql.contains("SELECT navigation_start_ts AS start_ts, fcp_ts AS end_ts"),
            "navigation_id without phase must default to navigation_to_fcp, got: {sql}",
        );
        assert!(
            sql.contains("WHERE navigation_id = 7 "),
            "navigation_id must match only chrome_page_loads.navigation_id, got: {sql}",
        );
        assert!(
            !sql.contains("WHERE id = 7"),
            "navigation_id must not also match page_loads.id, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_with_phase_without_id_uses_latest_page_load() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            phase: Some(ChromeMainThreadHotspotsPhase::DclToFcp),
            ..Default::default()
        })
        .expect("phase-only builder must succeed");
        assert!(
            sql.contains("SELECT dom_content_loaded_event_ts AS start_ts, fcp_ts AS end_ts"),
            "dcl_to_fcp phase must select DCL→FCP window, got: {sql}",
        );
        assert!(
            !sql.contains("WHERE id ="),
            "phase without page_load_id should use latest page load, got: {sql}",
        );
        assert!(
            sql.contains("ORDER BY navigation_start_ts DESC LIMIT 1"),
            "phase-only SQL must use latest page load deterministically, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_rejects_invalid_windows() {
        let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            page_load_id: Some(-1),
            ..Default::default()
        })
        .expect_err("negative page_load_id must error");
        assert!(err.to_string().contains("page_load_id"), "got: {err}");

        let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            navigation_id: Some(-1),
            ..Default::default()
        })
        .expect_err("negative navigation_id must error");
        assert!(err.to_string().contains("navigation_id"), "got: {err}");

        let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            page_load_id: Some(1),
            navigation_id: Some(7),
            ..Default::default()
        })
        .expect_err("page_load_id plus navigation_id must error");
        assert!(err.to_string().contains("mutually exclusive"), "got: {err}");

        let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            start_ts_ns: Some(2000),
            end_ts_ns: Some(1000),
            ..Default::default()
        })
        .expect_err("end before start must error");
        assert!(err.to_string().contains("end_ts_ns"), "got: {err}");
    }

    /// `min_dur_ms = 33.0` translates to `ct.dur > 33000000` ns. Default
    /// (`None`) preserves the legacy 16 ms threshold pinned by the no-filter
    /// test above.
    #[test]
    fn chrome_main_thread_hotspots_sql_with_min_dur_ms_emits_threshold() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            min_dur_ms: Some(33.0),
            ..Default::default()
        })
        .expect("min_dur_ms builder must succeed");
        assert!(
            sql.contains("AND ct.dur > 33000000"),
            "min_dur_ms must convert ms→ns, got: {sql}",
        );
        assert!(
            !sql.contains("AND ct.dur > 16000000"),
            "explicit min_dur_ms must replace the 16 ms default, got: {sql}",
        );
    }

    /// `min_dur_ms = 0.0` is the explicit "show me everything" path — emits
    /// `ct.dur > 0` so SQL still runs but only filters out zero-duration rows
    /// (which `chrome_tasks` shouldn't have anyway).
    #[test]
    fn chrome_main_thread_hotspots_sql_with_min_dur_ms_zero_emits_zero_threshold() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            min_dur_ms: Some(0.0),
            ..Default::default()
        })
        .expect("zero threshold must be accepted");
        assert!(sql.contains("AND ct.dur > 0"), "got: {sql}");
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_rejects_negative_min_dur_ms() {
        let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            min_dur_ms: Some(-1.0),
            ..Default::default()
        })
        .expect_err("negative min_dur_ms must error");
        assert!(
            err.to_string().contains("min_dur_ms"),
            "error must mention min_dur_ms, got: {err}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_rejects_nan_min_dur_ms() {
        let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            min_dur_ms: Some(f64::NAN),
            ..Default::default()
        })
        .expect_err("NaN min_dur_ms must error");
        assert!(
            err.to_string().contains("min_dur_ms"),
            "error must mention min_dur_ms, got: {err}",
        );
    }

    /// Pre-fix: `(1e20 * 1e6) as i64` saturates to `i64::MAX`, the SQL ran
    /// silently with `dur > 9223372036854775807`, and the LLM got an empty
    /// "good performance" result on a query that was meaningless. Post-fix:
    /// the overflow guard fires before the cast and surfaces the failure.
    #[test]
    fn chrome_main_thread_hotspots_sql_rejects_min_dur_ms_overflowing_i64_ns() {
        let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            min_dur_ms: Some(1e20),
            ..Default::default()
        })
        .expect_err("min_dur_ms that overflows i64 ns must error");
        assert!(
            err.to_string().contains("min_dur_ms"),
            "error must mention min_dur_ms, got: {err}",
        );
    }

    /// Positive-boundary counterpart to the overflow rejection. `9e12 ms`
    /// ≈ 285 years sits comfortably under `i64::MAX as f64 / 1e6` ≈ 9.22e12,
    /// so the guard accepts. Pins that the boundary is set permissively
    /// enough not to false-reject any real-world threshold.
    #[test]
    fn chrome_main_thread_hotspots_sql_accepts_min_dur_ms_just_under_i64_ns_overflow() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            min_dur_ms: Some(9e12),
            ..Default::default()
        })
        .expect("near-boundary min_dur_ms must accept");
        assert!(
            sql.contains("AND ct.dur > 9000000000000000000"),
            "9e12 ms must convert to 9e18 ns in the WHERE clause, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_with_limit_overrides_default() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            limit: Some(25),
            ..Default::default()
        })
        .expect("limit builder must succeed");
        assert!(
            sql.contains("ORDER BY ct.dur DESC LIMIT 25"),
            "explicit limit must replace LIMIT 100 default, got: {sql}",
        );
        assert!(
            !sql.contains("LIMIT 100"),
            "explicit limit must not coexist with default, got: {sql}",
        );
    }

    /// `limit > MAX_ROWS` clamps silently to 5000 — same rationale as
    /// `execute_sql`'s row cap (don't dump unbounded JSON to the LLM).
    #[test]
    fn chrome_main_thread_hotspots_sql_clamps_limit_to_max_rows() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            limit: Some(99_999),
            ..Default::default()
        })
        .expect("oversized limit must clamp, not error");
        assert!(
            sql.contains(&format!("LIMIT {MAX_ROWS}")),
            "limit must clamp to MAX_ROWS={MAX_ROWS}, got: {sql}",
        );
    }

    #[test]
    fn chrome_main_thread_hotspots_sql_rejects_zero_limit() {
        let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            limit: Some(0),
            ..Default::default()
        })
        .expect_err("limit=0 must error");
        assert!(
            err.to_string().contains("limit"),
            "error must mention limit, got: {err}",
        );
    }

    /// list_threads_in_process now accepts upid OR process_name. With neither
    /// set, it must surface a clear error eagerly (before any RPC).
    #[test]
    fn list_threads_in_process_requires_one_of_upid_or_process_name() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");
        runtime.block_on(async {
            let server = test_server();
            let r = server
                .list_threads_in_process(Parameters(ListThreadsInProcessParams {
                    upid: None,
                    process_name: None,
                }))
                .await;
            let err = r.expect_err("must reject when neither upid nor process_name is set");
            assert!(err.contains("upid"), "error must mention upid, got: {err}");
            assert!(
                err.contains("process_name"),
                "error must mention process_name, got: {err}",
            );
        });
    }

    #[test]
    fn table_list_serialize_shape() {
        let list = TableList {
            names: vec!["t1".into(), "t2".into()],
        };
        let value = serde_json::to_value(&list).expect("serialize");
        assert_eq!(value, json!({"names": ["t1", "t2"]}));
    }

    #[test]
    fn table_info_serialize_uses_renamed_type_field() {
        let info = TableInfo {
            table: "thread_slice".into(),
            columns: vec![ColumnInfo {
                name: "id".into(),
                data_type: "INTEGER".into(),
                nullable: false,
            }],
        };
        let value = serde_json::to_value(&info).expect("serialize");
        assert_eq!(
            value,
            json!({
                "table": "thread_slice",
                "columns": [{"name": "id", "type": "INTEGER", "nullable": false}],
            }),
            "ColumnInfo.data_type must serialize as `type` (serde rename)",
        );
    }

    /// PRAGMA table_info returns notnull = 0 for nullable, 1 for NOT NULL.
    /// `pragma_row_to_column_info` inverts that into a bool. Pin the inversion
    /// so no one flips the polarity by mistake. Calls the production helper
    /// directly so the test cannot drift away from the real projection logic.
    #[test]
    fn pragma_row_to_column_info_inverts_notnull() {
        let pragma = DecodedTable {
            columns: vec!["name".into(), "type".into(), "notnull".into()],
            rows: vec![
                vec![
                    serde_json::Value::from("a"),
                    serde_json::Value::from("INTEGER"),
                    serde_json::Value::from(0),
                ],
                vec![
                    serde_json::Value::from("b"),
                    serde_json::Value::from("TEXT"),
                    serde_json::Value::from(1),
                ],
            ],
        };
        let nullable_row = pragma_row_to_column_info(&pragma, 0).expect("row 0 valid");
        let not_null_row = pragma_row_to_column_info(&pragma, 1).expect("row 1 valid");
        assert_eq!(nullable_row.name, "a");
        assert_eq!(nullable_row.data_type, "INTEGER");
        assert!(
            nullable_row.nullable,
            "notnull = 0 must yield nullable = true",
        );
        assert_eq!(not_null_row.name, "b");
        assert_eq!(not_null_row.data_type, "TEXT");
        assert!(
            !not_null_row.nullable,
            "notnull = 1 must yield nullable = false",
        );
    }

    /// PRAGMA contract violations (missing `name` or `type`) must surface as
    /// errors rather than silently producing a `"?"` placeholder. The
    /// pre-v0.9.x code used `unwrap_or("?")` which made an upstream decoder
    /// or trace_processor regression invisible at the tool boundary.
    #[test]
    fn pragma_row_to_column_info_errors_on_missing_name() {
        let pragma = DecodedTable {
            columns: vec!["type".into(), "notnull".into()],
            rows: vec![vec![
                serde_json::Value::from("INTEGER"),
                serde_json::Value::from(0),
            ]],
        };
        let err = pragma_row_to_column_info(&pragma, 0)
            .expect_err("missing `name` column must surface as error");
        assert!(err.contains("missing `name` column"), "got: {err}");
        assert!(err.contains("contract violation"), "got: {err}");
    }

    #[test]
    fn pragma_row_to_column_info_errors_on_missing_type() {
        let pragma = DecodedTable {
            columns: vec!["name".into(), "notnull".into()],
            rows: vec![vec![
                serde_json::Value::from("a"),
                serde_json::Value::from(0),
            ]],
        };
        let err = pragma_row_to_column_info(&pragma, 0)
            .expect_err("missing `type` column must surface as error");
        assert!(err.contains("missing `type` column"), "got: {err}");
    }
}
