// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
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
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(STDLIB_INSTRUCTIONS.into()),
            ..Default::default()
        }
    }
}

#[tool_router(router = tool_router)]
impl PerfettoMcpServer {
    #[tool(
        name = "load_trace",
        description = "Load a Perfetto trace file for analysis. Every other tool operates on \
                       the trace set here.\n\
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
                       Perfetto LUCI bucket."
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
        // half-loaded trace.
        *self.current_trace.lock().await = Some(params.path);

        Ok(format!(
            "Trace loaded successfully: {display}\n\
             Use list_tables to see available tables, then \
             list_table_structure to see column details."
        ))
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
                       Requires `load_trace` to have run first.\n\
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
                       https://perfetto.dev/docs/analysis/perfetto-sql-syntax (syntax)."
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
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
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
                       Errors when: no trace is loaded — call `load_trace` first."
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
                       `list_tables` first if uncertain about the name."
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
                       Errors when: no trace is loaded — call `load_trace` first."
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
                       table directly."
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
                       Parameters: none — operates on the loaded trace.\n\
                       \n\
                       Empty result: no janky frames detected (clean trace) or no \
                       scrolls occurred during capture."
    )]
    async fn chrome_scroll_jank_summary(
        &self,
        Parameters(_params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome scroll jank summary").await?;
        let table = client
            .query(CHROME_SCROLL_JANK_SUMMARY_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome scroll jank summary", e))?;
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
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
                       Parameters: none — operates on the loaded trace.\n\
                       \n\
                       Empty result: no navigations occurred during capture (e.g. trace \
                       started after the page was already loaded)."
    )]
    async fn chrome_page_load_summary(
        &self,
        Parameters(_params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page load summary").await?;
        let table = client
            .query(CHROME_PAGE_LOAD_SUMMARY_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page load summary", e))?;
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
    }

    #[tool(
        name = "chrome_main_thread_hotspots",
        description = "Top Chrome main-thread tasks by wall duration: id, name, \
                       task_type, thread_name, process_name, dur_ms, cpu_pct \
                       (thread_dur/dur), thread_dur_ms. Uses `chrome.tasks` and \
                       `thread.is_main_thread = 1` (tid == pid per Linux convention).\n\
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
                         process type. `process_name='Renderer'` shows all renderers \
                         together; `pid` is the OS pid (visible in Task Manager but \
                         can be recycled mid-trace); `upid` is the trace-internal \
                         unique pid (always precise — prefer over `pid` for \
                         multi-renderer traces). Look up both via `list_processes`. \
                         All AND when set; redundant pairings (e.g. matching \
                         upid + pid) are harmless.\n\
                       - `min_dur_ms`: minimum task duration. Defaults to 16 (one \
                         60 Hz frame). Pass 0 for ALL tasks; raise to 33 (30 Hz) or \
                         100 to focus on bigger stutters.\n\
                       - `limit`: max rows (default 100, capped at 5000). Must be > 0 \
                         if set.\n\
                       \n\
                       Empty result: either no main-thread tasks exceeded `min_dur_ms` \
                       (good performance at that threshold), or thread metadata is \
                       incomplete (`is_main_thread` is NULL). If the latter is \
                       suspected, retry with `execute_sql` filtering on `thread_name \
                       IN ('CrBrowserMain', 'CrRendererMain')` to bypass the \
                       `is_main_thread` filter."
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
            min_dur_ms: params.min_dur_ms,
            limit: params.limit,
        })
        .map_err(|e| e.to_string())?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome main-thread hotspots", e))?;
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
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
                       Parameters: none — operates on the loaded trace.\n\
                       \n\
                       Empty result: trace started after the browser was already \
                       running (most cases — startup is captured only when tracing \
                       began before launch)."
    )]
    async fn chrome_startup_summary(
        &self,
        Parameters(_params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome startup summary").await?;
        let table = client
            .query(CHROME_STARTUP_SUMMARY_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome startup summary", e))?;
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
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
                       Parameters: none — operates on the loaded trace.\n\
                       \n\
                       Empty result: no interactions captured (trace started before \
                       user input or interaction tracking was disabled in tracing \
                       config)."
    )]
    async fn chrome_web_content_interactions(
        &self,
        Parameters(_params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome web content interactions").await?;
        let table = client
            .query(CHROME_WEB_CONTENT_INTERACTIONS_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome web content interactions", e))?;
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
    }

    #[tool(
        name = "list_stdlib_modules",
        description = "List a curated set of PerfettoSQL stdlib modules. Returns a JSON \
                       array — each entry has `module` (the value for `INCLUDE PERFETTO \
                       MODULE`), `domain` (chrome / android / generic), `views`, \
                       `description`, and an illustrative `usage` query.\n\
                       \n\
                       Use when: exploring what's available before writing SQL against \
                       an unfamiliar trace type, or discovering modules outside the \
                       dedicated `chrome_*` tools (memory, sched, wattson, v8, etc.). \
                       Call this before `load_trace` if you want to scope your analysis \
                       upfront — no trace needs to be loaded.\n\
                       \n\
                       Don't use for: discovering all stdlib modules — this is a \
                       curated subset of the most useful ones. The exhaustive list \
                       lives at https://perfetto.dev/docs/analysis/stdlib-docs.\n\
                       \n\
                       Parameters: none.\n\
                       \n\
                       Then use `execute_sql` with `INCLUDE PERFETTO MODULE <module>; \
                       SELECT ...` (both can be in one call). If `PERFETTO_TP_PATH` \
                       points to a custom binary, some modules may not exist in that \
                       version — verify column names with `list_table_structure` if a \
                       query fails."
    )]
    async fn list_stdlib_modules(
        &self,
        Parameters(_params): Parameters<ListStdlibModulesParams>,
    ) -> Result<String, String> {
        Ok(STDLIB_MODULE_LIST.to_owned())
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

/// Validate a string for use in SQL GLOB patterns or table names.
///
/// Only allows alphanumeric characters and `._-:*?` to prevent injection.
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

    // Without this capability, clients skip `tools/list` on handshake and no
    // tools are registered — the router still has them, but they're invisible.
    #[test]
    fn get_info_declares_tools_capability() {
        let info = test_server().get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "server must declare `tools` capability so clients call tools/list"
        );
    }

    #[test]
    fn instructions_list_core_stdlib_modules() {
        let info = test_server().get_info();
        let instructions = info
            .instructions
            .expect("server must ship instructions with stdlib module directory");
        for module in [
            "chrome.page_loads",
            "chrome.scroll_jank.scroll_jank_v3",
            "chrome.tasks",
            "chrome.startups",
            "android.startup.startups",
            "android.anrs",
            "android.binder",
            "slices.with_context",
        ] {
            assert!(
                instructions.contains(module),
                "instructions missing stdlib module `{module}` — agents will fall back to raw slice scans"
            );
        }
        assert!(
            instructions.contains("INCLUDE PERFETTO MODULE"),
            "instructions must tell agents to INCLUDE stdlib modules before querying"
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
            ],
        );
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
                .chrome_scroll_jank_summary(Parameters(ChromeTraceParams {}))
                .await;
            let err = r
                .map(|_| ())
                .expect_err("chrome_scroll_jank_summary: preflight must reject");
            assert!(err.contains("Chrome scroll jank summary"), "got: {err}");
            assert!(err.contains("Chrome-family trace"), "got: {err}");
            assert!(err.contains("list_stdlib_modules"), "got: {err}");

            let r = server
                .chrome_page_load_summary(Parameters(ChromeTraceParams {}))
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
                    min_dur_ms: None,
                    limit: None,
                }))
                .await;
            let err = r
                .map(|_| ())
                .expect_err("chrome_main_thread_hotspots: preflight must reject");
            assert!(err.contains("Chrome main-thread hotspots"), "got: {err}");
            assert!(err.contains("Chrome-family trace"), "got: {err}");
            assert!(err.contains("list_stdlib_modules"), "got: {err}");

            let r = server
                .chrome_startup_summary(Parameters(ChromeTraceParams {}))
                .await;
            let err = r
                .map(|_| ())
                .expect_err("chrome_startup_summary: preflight must reject");
            assert!(err.contains("Chrome startup summary"), "got: {err}");
            assert!(err.contains("Chrome-family trace"), "got: {err}");
            assert!(err.contains("list_stdlib_modules"), "got: {err}");

            let r = server
                .chrome_web_content_interactions(Parameters(ChromeTraceParams {}))
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
        let p: ChromeMainThreadHotspotsParams =
            serde_json::from_str(r#"{"pid": "12800", "min_dur_ms": "50", "limit": "30"}"#)
                .expect("stringified numerics must deserialize after v0.11.3");
        assert_eq!(p.pid, Some(12800));
        assert_eq!(p.min_dur_ms, Some(50.0));
        assert_eq!(p.limit, Some(30));
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
            ("min_dur_ms", "number"),
            ("limit", "integer"),
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
    /// prone to hallucinate fields. The 9 advertised tools all carry params
    /// with `deny_unknown_fields` (`ListStdlibModulesParams` is empty but
    /// still closed — that closure is the whole reason the empty type
    /// exists; rmcp's parameterless `async fn foo(&self)` shape, by contrast,
    /// emits an *open* schema and silently ignores hallucinated fields).
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

    /// No-filter SQL keeps the same `JOIN process p ON ct.upid = p.upid` clause
    /// as the filtered variants, so the join is harmless when no pid filter is
    /// set — this means handlers can always use the same builder.
    #[test]
    fn chrome_main_thread_hotspots_sql_no_filter_runs_all_main_threads() {
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters::default())
            .expect("builder must succeed");
        assert!(sql.contains("WHERE t.is_main_thread = 1"));
        assert!(sql.contains("AND ct.dur > 16000000"));
        assert!(sql.contains("ORDER BY ct.dur DESC LIMIT 100"));
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
