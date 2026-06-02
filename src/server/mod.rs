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
                       URL, raw boundary timestamps, FCP / LCP / DCL / load timings \
                       in ms. Read-only.\n\
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
        name = "chrome_page_load_resource_hotspots",
        description = "Rank URL-bearing Chrome resource/request slices in a page-load/raw \
                       window. Returns slice timing, overlap_ms/pct_of_window, \
                       process/thread, URL. Use after `chrome_page_load_resource_summary` \
                       to drill into the concrete Renderer/NetworkService/async slice \
                       behind a slow URL. Filters: page_load_id/navigation_id/phase, raw \
                       start/end ns, min_dur_ms default 50, limit, max_string_len.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_page_load_resource_hotspots(
        &self,
        Parameters(params): Parameters<ChromePageLoadResourceHotspotsParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page-load resource hotspots").await?;
        let sql = chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters {
            window: ChromePageLoadWindowFilters {
                page_load_id: params.page_load_id,
                navigation_id: params.navigation_id,
                phase: params.phase,
                start_ts_ns: params.start_ts_ns,
                end_ts_ns: params.end_ts_ns,
            },
            min_dur_ms: params.min_dur_ms,
            limit: params.limit,
        })
        .map_err(|e| e.to_string())?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page-load resource hotspots", e))?;
        format_chrome_tool_response(
            table,
            chrome_hotspots_effective_limit(params.limit),
            params.max_string_len,
        )
    }

    #[tool(
        name = "chrome_page_load_resource_summary",
        description = "URL-level Chrome resource/request summary for a page-load/raw \
                       window. Returns URL key, slice/process/priority sets, \
                       first/last/span, max_overlap_ms, summed_overlap_ms, \
                       pct_of_window, example_slice_id, and attribution evidence. \
                       Use before `chrome_page_load_resource_hotspots` for slow \
                       FCP/load; rank by max overlap because summed overlap can \
                       double-count layered slices.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_page_load_resource_summary(
        &self,
        Parameters(params): Parameters<ChromePageLoadResourceSummaryParams>,
    ) -> Result<String, String> {
        let window = ChromePageLoadWindowFilters {
            page_load_id: params.page_load_id,
            navigation_id: params.navigation_id,
            phase: params.phase,
            start_ts_ns: params.start_ts_ns,
            end_ts_ns: params.end_ts_ns,
        };
        let sql = chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters {
            window,
            min_overlap_ms: params.min_overlap_ms,
            url_grouping: params.url_grouping,
            limit: params.limit,
        })
        .map_err(|e| e.to_string())?;
        let evidence_sql =
            chrome_page_load_resource_timing_evidence_sql(window).map_err(|e| e.to_string())?;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page-load resource summary").await?;
        let evidence_table = client.query(&evidence_sql).await.map_err(|e| {
            format_chrome_tool_error("Chrome page-load resource summary evidence", e)
        })?;
        let evidence = chrome_resource_timing_evidence_from_probe(&evidence_table);
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page-load resource summary", e))?;
        format_chrome_resource_summary_response(
            table,
            chrome_hotspots_effective_limit_with_default(params.limit, 25),
            params.max_string_len,
            evidence,
        )
    }

    #[tool(
        name = "chrome_page_load_resource_pipeline",
        description = "Drill into one Chrome page-load resource URL and join its \
                       lifecycle/request spans with script parse/evaluate and \
                       style/layout signals. Use after \
                       `chrome_page_load_resource_summary` by passing \
                       `example_slice_id` or `url_substring` for a slow URL. \
                       Returns request/resource timing facts plus an \
                       explicit `matched_by`/`matched_url_seed` so callers can \
                       verify why the row matched, plus an \
                       evidence_boundary reminding callers not to label \
                       DNS/TLS/TTFB/download/cache without phase-specific rows. \
                       Parameters: `url_substring` or `example_slice_id` required; \
                       optional page-load/window filters, `url_grouping`, `limit` \
                       default 30, `max_string_len`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_page_load_resource_pipeline(
        &self,
        Parameters(params): Parameters<ChromePageLoadResourcePipelineParams>,
    ) -> Result<String, String> {
        let sql = chrome_page_load_resource_pipeline_sql(ChromePageLoadResourcePipelineFilters {
            window: ChromePageLoadWindowFilters {
                page_load_id: params.page_load_id,
                navigation_id: params.navigation_id,
                phase: params.phase,
                start_ts_ns: params.start_ts_ns,
                end_ts_ns: params.end_ts_ns,
            },
            url_substring: params.url_substring.as_deref(),
            example_slice_id: params.example_slice_id,
            url_grouping: params.url_grouping,
            limit: params.limit,
        })
        .map_err(|e| e.to_string())?;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page-load resource pipeline").await?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page-load resource pipeline", e))?;
        format_chrome_tool_response(
            table,
            chrome_hotspots_effective_limit_with_default(params.limit, 30),
            params.max_string_len,
        )
    }

    #[tool(
        name = "chrome_page_load_script_hotspots",
        description = "Rank renderer main-thread script groups in a Chrome \
                       page-load/raw window: URL/name/process/thread, wall/CPU totals, \
                       style/layout ms, example_slice_id. Read-only.\n\
                       \n\
                       Use when: slow FCP/load needs post-resource JS attribution; expand \
                       `example_slice_id` with `slice_descendants_breakdown`.\n\
                       \n\
                       Parameters: optional process filters, page-load/window filters shared \
                       with `chrome_main_thread_hotspots`, `min_total_ms` (default 20), \
                       `limit`, `max_string_len`. Empty result: no matching script groups.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn chrome_page_load_script_hotspots(
        &self,
        Parameters(params): Parameters<ChromePageLoadScriptHotspotsParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page-load script hotspots").await?;
        let sql = chrome_page_load_script_hotspots_sql(ChromePageLoadScriptHotspotsFilters {
            process_name: params.process_name.as_deref(),
            pid: params.pid,
            upid: params.upid,
            window: ChromePageLoadWindowFilters {
                page_load_id: params.page_load_id,
                navigation_id: params.navigation_id,
                phase: params.phase,
                start_ts_ns: params.start_ts_ns,
                end_ts_ns: params.end_ts_ns,
            },
            min_total_ms: params.min_total_ms,
            limit: params.limit,
        })
        .map_err(|e| e.to_string())?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page-load script hotspots", e))?;
        format_chrome_tool_response(
            table,
            chrome_hotspots_effective_limit(params.limit),
            params.max_string_len,
        )
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
struct ChromeResourceTimingEvidence {
    attribution_scope: &'static str,
    phase_breakdown: &'static str,
    phase_breakdown_available: bool,
    safe_conclusion: &'static str,
    safe_fact_fields: Vec<&'static str>,
    unsafe_inferences: Vec<&'static str>,
    hypothesis_only: Vec<&'static str>,
    network_phase_slice_count: i64,
    network_phase_arg_count: i64,
    incomplete_resource_slice_count: i64,
    incomplete_slices_excluded: bool,
}

#[derive(Debug, Serialize)]
struct ChromeResourceSummaryRowsResponse {
    columns: Vec<String>,
    row_count: Option<usize>,
    returned_rows: usize,
    truncated: bool,
    row_count_known: bool,
    string_truncated: bool,
    redacted: bool,
    resource_timing_evidence: ChromeResourceTimingEvidence,
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
    chrome_hotspots_effective_limit_with_default(limit, DEFAULT_CHROME_TOOL_ROWS)
}

fn chrome_hotspots_effective_limit_with_default(limit: Option<u32>, default_limit: usize) -> usize {
    match limit {
        Some(n) if (n as usize) > MAX_ROWS => MAX_ROWS,
        Some(n) => n as usize,
        None => default_limit,
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

fn format_chrome_resource_summary_response(
    table: DecodedTable,
    effective_limit: usize,
    max_string_len: Option<u32>,
    evidence: ChromeResourceTimingEvidence,
) -> Result<String, String> {
    format_chrome_resource_summary_response_with_redaction(
        table,
        effective_limit,
        tool_max_string_len(max_string_len)?,
        default_redact_strings(),
        evidence,
    )
}

fn format_chrome_resource_summary_response_with_redaction(
    table: DecodedTable,
    effective_limit: usize,
    max_string_len: Option<usize>,
    redact_strings: bool,
    evidence: ChromeResourceTimingEvidence,
) -> Result<String, String> {
    let shape = ExecuteSqlOutputShape {
        mode: ExecuteSqlOutputMode::FullRows,
        active: true,
        max_string_len,
        redact_strings,
    };
    let returned_rows = table.rows.len();
    let (rows, string_truncated, redacted) = transform_rows(table.rows.iter(), shape);
    serde_json::to_string(&ChromeResourceSummaryRowsResponse {
        columns: table.columns,
        row_count: None,
        returned_rows,
        truncated: effective_limit > 0 && returned_rows >= effective_limit,
        row_count_known: false,
        string_truncated,
        redacted,
        resource_timing_evidence: evidence,
        note: CHROME_TOOL_SHAPING_NOTE,
        rows,
    })
    .map_err(|e| format!("Failed to serialize results: {e}"))
}

fn chrome_resource_timing_evidence_from_probe(
    table: &DecodedTable,
) -> ChromeResourceTimingEvidence {
    let network_phase_slice_count =
        decoded_table_i64_cell(table, "network_phase_slice_count").unwrap_or(0);
    let network_phase_arg_count =
        decoded_table_i64_cell(table, "network_phase_arg_count").unwrap_or(0);
    let incomplete_resource_slice_count =
        decoded_table_i64_cell(table, "incomplete_resource_slice_count").unwrap_or(0);
    let phase_breakdown_available =
        decoded_table_i64_cell(table, "phase_breakdown_available").unwrap_or(0) > 0;

    if phase_breakdown_available {
        ChromeResourceTimingEvidence {
            attribution_scope: "url_lifecycle_span_with_phase_hints",
            phase_breakdown: "phase_hints_present",
            phase_breakdown_available: true,
            safe_conclusion: "Summary rows rank URL lifecycle/request spans; phase-like trace signals exist, so inspect phase rows before assigning DNS/TLS/TTFB/download/cache cause.",
            safe_fact_fields: vec![
                "url lifecycle/request span",
                "window overlap",
                "process/thread/upid evidence",
                "renderer/navigation relatedness",
            ],
            unsafe_inferences: vec![
                "dns",
                "tls",
                "ttfb",
                "download",
                "cache",
                "cdn",
                "server_response",
            ],
            hypothesis_only: vec![
                "cache/proxy delay",
                "cdn/server latency",
                "network bandwidth",
                "http2 priority/connection contention",
            ],
            network_phase_slice_count,
            network_phase_arg_count,
            incomplete_resource_slice_count,
            incomplete_slices_excluded: true,
        }
    } else {
        ChromeResourceTimingEvidence {
            attribution_scope: "url_lifecycle_span",
            phase_breakdown: "absent",
            phase_breakdown_available: false,
            safe_conclusion: "These URLs have long resource/request lifecycle spans overlapping the selected window.",
            safe_fact_fields: vec![
                "url lifecycle/request span",
                "window overlap",
                "process/thread/upid evidence",
                "renderer/navigation relatedness",
            ],
            unsafe_inferences: vec![
                "dns",
                "tls",
                "ttfb",
                "download",
                "cache",
                "cdn",
                "server_response",
            ],
            hypothesis_only: vec![
                "cache/proxy delay",
                "cdn/server latency",
                "network bandwidth",
                "http2 priority/connection contention",
            ],
            network_phase_slice_count,
            network_phase_arg_count,
            incomplete_resource_slice_count,
            incomplete_slices_excluded: true,
        }
    }
}

fn decoded_table_i64_cell(table: &DecodedTable, col: &str) -> Option<i64> {
    table.cell(0, col).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
    })
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
            "chrome_page_load_resource_summary",
            "chrome_page_load_resource_pipeline",
            "chrome_page_load_resource_hotspots",
            "chrome_page_load_script_hotspots",
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
mod tests;
