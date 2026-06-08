// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult,
        ResourceContents, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use tokio::sync::Mutex;

use crate::params::*;
use crate::sql_templates::*;
use crate::stdlib_catalog::{STDLIB_QUICKREF, STDLIB_QUICKREF_MIME_TYPE, STDLIB_QUICKREF_URI};
use crate::tp_client::TraceProcessorClient;
use crate::tp_manager::{
    trace_file_platform_fingerprint, trace_file_sample_sha256, TraceProcessorManager,
};

#[cfg(test)]
use crate::error::{PerfettoError, QueryErrorKind, MAX_ROWS};
use crate::query::DecodedTable;
#[cfg(test)]
use crate::stdlib_catalog::STDLIB_MODULE_LIST;

mod chrome;
mod errors;
mod resources;
mod response;
mod schema;
mod trace_summary;

use chrome::*;
use errors::*;
use resources::*;
use response::*;
use schema::*;
use trace_summary::*;

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
    schema_cache: Arc<Mutex<SchemaCache>>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaCacheTraceKey {
    canonical_path: PathBuf,
    size_bytes: u64,
    modified: Option<SystemTime>,
    platform: Option<String>,
    sample_sha256: String,
}

#[derive(Debug, Default)]
struct SchemaCache {
    trace_key: Option<SchemaCacheTraceKey>,
    table_structures: HashMap<String, String>,
}

impl SchemaCache {
    fn table_structure(
        &mut self,
        trace_key: &SchemaCacheTraceKey,
        table_name: &str,
    ) -> Option<String> {
        self.ensure_trace(trace_key);
        self.table_structures.get(table_name).cloned()
    }

    fn store_table_structure(
        &mut self,
        trace_key: SchemaCacheTraceKey,
        table_name: String,
        response: String,
    ) {
        self.ensure_trace(&trace_key);
        self.table_structures.insert(table_name, response);
    }

    fn ensure_trace(&mut self, trace_key: &SchemaCacheTraceKey) {
        if self.trace_key.as_ref() != Some(trace_key) {
            self.trace_key = Some(trace_key.clone());
            self.table_structures.clear();
        }
    }
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

fn decoded_row_count(table: &crate::query::DecodedTable, tool_name: &str) -> Result<usize, String> {
    decoded_table_i64_cell(table, "row_count")
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| format!("{tool_name} count query did not return a valid row_count"))
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
                       near-zero-cost unless the same path's metadata/content fingerprint \
                       changed since it was loaded.\n\
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "load_trace", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn load_trace(
        &self,
        Parameters(params): Parameters<LoadTraceParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!("path_len={}", params.path.len()))
            .await;
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
                       SQL. Blob cells render as `blob:hex:<hex>`. String results \
                       may be redacted by the server privacy \
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "execute_sql", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn execute_sql(
        &self,
        Parameters(params): Parameters<ExecuteSqlParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "sql_kind={},sql_len={},shaping={}",
            crate::telemetry::sql_span_kind(&params.sql),
            params.sql.len(),
            params.columns_only
                || params.summary
                || params.head.is_some()
                || params.limit.is_some()
                || params.include_row_count
                || params.max_string_len.is_some()
        ))
        .await;
        let decode_options = execute_sql_decode_options(&params)?;
        let client = self.client_for_current().await?;
        let decoded = client
            .query_with_options(&params.sql, decode_options)
            .await
            .map_err(format_execute_sql_error)?;
        format_execute_sql_decoded_response_with_redaction(
            decoded,
            &params,
            default_redact_strings(),
        )
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "list_tables", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn list_tables(
        &self,
        Parameters(params): Parameters<ListTablesParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!("pattern_set={}", params.pattern.is_some()))
            .await;
        let trace_path = self.current_trace_path().await?;
        let pattern_key = match &params.pattern {
            Some(pat) => Some(sanitize_glob_param(pat).map_err(|e| e.to_string())?),
            None => None,
        };

        let client = self.client_for(&trace_path).await?;

        let sql = match &pattern_key {
            Some(pat) => {
                format!(
                    "SELECT name FROM sqlite_master \
                     WHERE type IN ('table', 'view') AND name GLOB '{pat}' \
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
                       primary_key flag.\n\
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "list_table_structure", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn list_table_structure(
        &self,
        Parameters(params): Parameters<TableStructureParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!("table_name_len={}", params.table_name.len()))
            .await;
        let trace_path = self.current_trace_path().await?;
        let trace_key = trace_schema_cache_key(&trace_path)?;
        reject_table_structure_pattern(&params.table_name)?;
        let table_name = sanitize_glob_param(&params.table_name).map_err(|e| e.to_string())?;
        if let Some(cached) = self
            .schema_cache
            .lock()
            .await
            .table_structure(&trace_key, &table_name)
        {
            return Ok(cached);
        }
        let client = self.client_for(&trace_path).await?;

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

        let response = serde_json::to_string(&TableInfo {
            table: table_name.clone(),
            columns,
        })
        .map_err(|e| format!("Failed to serialize results: {e}"))?;
        self.schema_cache.lock().await.store_table_structure(
            trace_key,
            table_name,
            response.clone(),
        );
        Ok(response)
    }

    #[tool(
        name = "list_processes",
        description = "List every process captured in the trace: upid (trace-internal \
                       id), pid, machine_id, name, start_ts, end_ts. Read-only.\n\
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "list_processes", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn list_processes(
        &self,
        Parameters(_params): Parameters<ListProcessesParams>,
    ) -> Result<String, String> {
        self.record_tool_span("no_params".to_owned()).await;
        let client = self.client_for_current().await?;
        let machine_id_expr = if table_has_column(&client, "process", "machine_id").await? {
            "machine_id"
        } else {
            "NULL"
        };
        let sql = format!(
            "SELECT upid, pid, {machine_id_expr} AS machine_id, name, start_ts, end_ts \
             FROM process ORDER BY start_ts"
        );
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format!("Failed to list processes: {e}"))?;
        serde_json::to_string(&table).map_err(|e| format!("Failed to serialize results: {e}"))
    }

    #[tool(
        name = "list_threads_in_process",
        description = "List threads in one process or same-named process set: \
                       tid, thread_name, pid, upid, machine_id. Limit 2000, cap 5000.\n\
                       \n\
                       Use when: drilling into a process from `list_processes`.\n\
                       \n\
                       Don't use for: ALL trace threads — use `execute_sql` on `thread`.\n\
                       \n\
                       Parameters: pass either `upid` (trace-internal id, precise — \
                       prefer when multiple processes share a name like 'Renderer') or \
                       `process_name` (exact match). `upid` wins when both are set. \
                       Optional `limit` and `offset` page large result sets; both \
                       accept numbers or numeric strings.\n\
                       \n\
                       Output: exact `row_count`, `returned_rows`, `truncated`/`has_more`; \
                       rows are ordered by pid/tid. \
                       `process_counts` reports per-upid counts for same-name fan-out.\n\
                       \n\
                       Empty result: returned as an error pointing at `list_processes` \
                       for available candidates.\n\
                       \n\
                       When `truncated=true`, increase `offset` or drill down with `upid`.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "list_threads_in_process", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn list_threads_in_process(
        &self,
        Parameters(params): Parameters<ListThreadsInProcessParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "upid_set={},process_name_set={},limit_set={},offset_set={}",
            params.upid.is_some(),
            params.process_name.is_some(),
            params.limit.is_some(),
            params.offset.is_some()
        ))
        .await;
        // Validate inputs BEFORE opening the trace — failing fast on bad
        // params avoids spawning trace_processor_shell for a request that
        // can't possibly succeed.
        // LIMIT keeps us clear of the 5000-row hard cap on Chrome renderer-fork
        // and Android system_server traces where a single process name can
        // fan out to thousands of threads.
        let (from_where, selector_for_error) = match (params.upid, &params.process_name) {
            (Some(upid), _) => (
                format!(
                    "FROM thread t JOIN process p ON t.upid = p.upid \
                     WHERE p.upid = {upid}"
                ),
                format!("upid {upid}"),
            ),
            (None, Some(name)) => {
                let name_lit = sql_string_literal(name).map_err(|e| e.to_string())?;
                (
                    format!(
                        "FROM thread t JOIN process p ON t.upid = p.upid \
                         WHERE p.name = {name_lit}"
                    ),
                    format!("process name {name:?}"),
                )
            }
            (None, None) => {
                return Err("Either `upid` or `process_name` must be provided.".to_string());
            }
        };
        let client = self.client_for_current().await?;
        let process_has_machine_id = table_has_column(&client, "process", "machine_id").await?;
        let machine_id_expr = if process_has_machine_id {
            "p.machine_id"
        } else {
            "NULL"
        };
        let count_sql = format!("SELECT COUNT(*) AS row_count {from_where}");
        let count_table = client
            .query(&count_sql)
            .await
            .map_err(|e| format!("Failed to count threads: {e}"))?;
        let row_count = decoded_row_count(&count_table, "list_threads_in_process")?;
        if row_count == 0 {
            return Err(format!(
                "No threads found for {selector_for_error}. Call list_processes \
                 to see available processes."
            ));
        }
        let process_counts_sql = format!(
            "SELECT p.pid, p.upid, {machine_id_expr} AS machine_id, \
                    p.name AS process_name, COUNT(*) AS thread_count \
             {from_where} \
             GROUP BY p.pid, p.upid, {machine_id_expr}, p.name \
             ORDER BY p.pid, p.upid"
        );
        let process_counts_table = client
            .query(&process_counts_sql)
            .await
            .map_err(|e| format!("Failed to count threads by process: {e}"))?;
        let process_counts = decode_list_threads_process_counts(&process_counts_table)?;
        let effective_limit = bounded_tool_limit(params.limit, LIST_THREADS_LIMIT)?;
        let offset = params.offset.unwrap_or(0) as usize;
        let sql = format!(
            "SELECT t.tid, t.name AS thread_name, p.pid, p.upid, \
                    {machine_id_expr} AS machine_id \
             {from_where} \
             ORDER BY p.pid, t.tid \
             LIMIT {effective_limit} OFFSET {offset}"
        );
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format!("Failed to list threads: {e}"))?;
        format_list_threads_response(table, row_count, offset, effective_limit, process_counts)
    }

    #[tool(
        name = "chrome_scroll_jank_summary",
        description = "Summarize the worst scroll jank frames in a Chrome trace: \
                       cause_of_jank, sub_cause_of_jank, delay_since_last_frame, \
                       event_latency_id, scroll_id, vsync_interval. One row per janky \
                       frame, sorted by delay_since_last_frame DESC. \
                       Read-only.\n\
                       \n\
                       Use when: investigating jank reports, finding scroll regressions, \
                       ranking jank causes. Prefer over hand-rolling SQL on \
                       `chrome.scroll_jank.scroll_jank_v3` — same data, less code.\n\
                       \n\
                       Don't use for: non-Chrome traces (will error). For custom \
                       filters, use `execute_sql` against the same view.\n\
                       \n\
                       Parameters: optional `limit` (default 100, capped at 5000) and \
                       `max_string_len`. Operates on the loaded trace.\n\
                       \n\
                       Output: metadata-first JSON; `row_count` exact; \
                       `truncated=true` means more rows exist; \
                       `string_truncated=true` means shortened text.\n\
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_scroll_jank_summary", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_scroll_jank_summary(
        &self,
        Parameters(params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "limit_set={},max_string_len_set={}",
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let effective_limit = bounded_tool_limit(params.limit, DEFAULT_CHROME_TOOL_ROWS)?;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome scroll jank summary").await?;
        let table = client
            .query(&chrome_scroll_jank_summary_sql(effective_limit))
            .await
            .map_err(|e| format_chrome_tool_error("Chrome scroll jank summary", e))?;
        let count_table = client
            .query(CHROME_SCROLL_JANK_SUMMARY_COUNT_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome scroll jank summary count", e))?;
        let row_count = decoded_row_count(&count_table, "Chrome scroll jank summary")?;
        format_chrome_tool_response_with_known_row_count(table, row_count, params.max_string_len)
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
                       Parameters: optional `limit` (default 100, capped at 5000) and \
                       `max_string_len`. Operates on the loaded trace.\n\
                       \n\
                       Output: metadata-first JSON; `row_count` exact; \
                       `truncated=true` means more rows exist; \
                       `string_truncated=true` means shortened text.\n\
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_page_load_summary", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_page_load_summary(
        &self,
        Parameters(params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "limit_set={},max_string_len_set={}",
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let effective_limit = bounded_tool_limit(params.limit, DEFAULT_CHROME_TOOL_ROWS)?;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page load summary").await?;
        let table = client
            .query(&chrome_page_load_summary_sql(effective_limit))
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page load summary", e))?;
        let count_table = client
            .query(CHROME_PAGE_LOAD_SUMMARY_COUNT_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page load summary count", e))?;
        let row_count = decoded_row_count(&count_table, "Chrome page load summary")?;
        format_chrome_tool_response_with_known_row_count(table, row_count, params.max_string_len)
    }

    #[tool(
        name = "chrome_page_load_resource_hotspots",
        description = "Rank URL-bearing Chrome resource/request slices in a page-load/raw \
                       window. Returns slice timing, overlap, process/thread, URL. \
                       Use after `chrome_page_load_resource_summary` to drill into \
                       slow URL slices. Filters: page_load/window, min_dur_ms default \
                       50, limit, max_string_len. \
                       `slice_duration_status='incomplete_duration'` means dur=-1; \
                       overlap is measured to window end or trace_end().",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_page_load_resource_hotspots", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_page_load_resource_hotspots(
        &self,
        Parameters(params): Parameters<ChromePageLoadResourceHotspotsParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "window_set={},min_dur_ms_set={},limit_set={},max_string_len_set={}",
            params.page_load_id.is_some()
                || params.navigation_id.is_some()
                || params.phase.is_some()
                || params.start_ts_ns.is_some()
                || params.end_ts_ns.is_some(),
            params.min_dur_ms.is_some(),
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page-load resource hotspots").await?;
        let effective_limit = bounded_tool_limit(params.limit, DEFAULT_CHROME_TOOL_ROWS)?;
        let probe_limit = extra_row_probe_limit(effective_limit);
        let sql = chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters {
            window: ChromePageLoadWindowFilters {
                page_load_id: params.page_load_id,
                navigation_id: params.navigation_id,
                phase: params.phase,
                start_ts_ns: params.start_ts_ns,
                end_ts_ns: params.end_ts_ns,
            },
            min_dur_ms: params.min_dur_ms,
            limit: Some(probe_limit as u32),
        })
        .map_err(|e| e.to_string())?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page-load resource hotspots", e))?;
        format_chrome_tool_response_with_probe_limit(table, effective_limit, params.max_string_len)
    }

    #[tool(
        name = "chrome_page_load_resource_summary",
        description = "URL-level Chrome resource/request summary for a page-load/raw \
                       window. Returns URL key, process/priority sets, span, \
                       max/summed overlap, navigation/renderer relation evidence, \
                       example_slice_id, incomplete_duration_slice_count. Use before \
                       `chrome_page_load_resource_hotspots`; rank by max overlap.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_page_load_resource_summary", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_page_load_resource_summary(
        &self,
        Parameters(params): Parameters<ChromePageLoadResourceSummaryParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "window_set={},min_overlap_ms_set={},url_grouping_set={},limit_set={},max_string_len_set={}",
            params.page_load_id.is_some()
                || params.navigation_id.is_some()
                || params.phase.is_some()
                || params.start_ts_ns.is_some()
                || params.end_ts_ns.is_some(),
            params.min_overlap_ms.is_some(),
            params.url_grouping.is_some(),
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let window = ChromePageLoadWindowFilters {
            page_load_id: params.page_load_id,
            navigation_id: params.navigation_id,
            phase: params.phase,
            start_ts_ns: params.start_ts_ns,
            end_ts_ns: params.end_ts_ns,
        };
        let effective_limit = bounded_tool_limit(params.limit, 25)?;
        let probe_limit = extra_row_probe_limit(effective_limit);
        let sql = chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters {
            window,
            min_overlap_ms: params.min_overlap_ms,
            url_grouping: params.url_grouping,
            limit: Some(probe_limit as u32),
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
        format_chrome_resource_summary_response_with_probe_limit(
            table,
            effective_limit,
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
                       Returns timing facts, `matched_by`/`matched_url_seed`, \
                       `incomplete_duration_resource_slice_count` and an \
                       evidence_boundary reminding callers not to label \
                       DNS/TLS/TTFB/cache without phase-specific rows. \
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_page_load_resource_pipeline", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_page_load_resource_pipeline(
        &self,
        Parameters(params): Parameters<ChromePageLoadResourcePipelineParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "window_set={},url_substring_set={},example_slice_id_set={},url_grouping_set={},limit_set={},max_string_len_set={}",
            params.page_load_id.is_some()
                || params.navigation_id.is_some()
                || params.phase.is_some()
                || params.start_ts_ns.is_some()
                || params.end_ts_ns.is_some(),
            params.url_substring.is_some(),
            params.example_slice_id.is_some(),
            params.url_grouping.is_some(),
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let effective_limit = bounded_tool_limit(params.limit, 30)?;
        let probe_limit = extra_row_probe_limit(effective_limit);
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
            limit: Some(probe_limit as u32),
        })
        .map_err(|e| e.to_string())?;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page-load resource pipeline").await?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page-load resource pipeline", e))?;
        format_chrome_tool_response_with_probe_limit(table, effective_limit, params.max_string_len)
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_page_load_script_hotspots", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_page_load_script_hotspots(
        &self,
        Parameters(params): Parameters<ChromePageLoadScriptHotspotsParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "process_filter_set={},window_set={},min_total_ms_set={},limit_set={},max_string_len_set={}",
            params.process_name.is_some() || params.pid.is_some() || params.upid.is_some(),
            params.page_load_id.is_some()
                || params.navigation_id.is_some()
                || params.phase.is_some()
                || params.start_ts_ns.is_some()
                || params.end_ts_ns.is_some(),
            params.min_total_ms.is_some(),
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome page-load script hotspots").await?;
        let effective_limit = bounded_tool_limit(params.limit, DEFAULT_CHROME_TOOL_ROWS)?;
        let probe_limit = extra_row_probe_limit(effective_limit);
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
            limit: Some(probe_limit as u32),
        })
        .map_err(|e| e.to_string())?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome page-load script hotspots", e))?;
        format_chrome_tool_response_with_probe_limit(table, effective_limit, params.max_string_len)
    }

    #[tool(
        name = "chrome_main_thread_hotspots",
        description = "Top Chrome main-thread tasks by wall duration: id, ts, \
                       name, task_type, thread_name, process_name, upid, pid, \
                       dur_ms, overlap_dur_ms, cpu_pct/thread_dur_ms (full-task), \
                       overlap_cpu_pct/overlap_thread_dur_ms (window estimates). \
                       Uses `chrome.tasks`, \
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
                         / `end_ts` are accepted; intersect page-load windows. \
                         `overlap_dur_ms` is clipped to that window.\n\
                       - `min_dur_ms`: minimum full-task duration, or clipped \
                         overlap duration when a window is set. Defaults to 16 \
                         ms. Pass 0 for all positive-overlap tasks.\n\
                        - `limit`: max rows (default 100, capped at 5000). Must be > 0 \
                          if set.\n\
                        - `max_string_len`: optional cap for returned string cells. \
                          Unset preserves full strings for precision. Must be > 0 if set.\n\
                        \n\
                        Output: metadata-first JSON preserving `columns` / \
                        `rows`; `truncated=true` means an extra-row probe found more rows; \
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_main_thread_hotspots", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_main_thread_hotspots(
        &self,
        Parameters(params): Parameters<ChromeMainThreadHotspotsParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "process_filter_set={},window_set={},min_dur_ms_set={},limit_set={},max_string_len_set={}",
            params.process_name.is_some() || params.pid.is_some() || params.upid.is_some(),
            params.page_load_id.is_some()
                || params.navigation_id.is_some()
                || params.phase.is_some()
                || params.start_ts_ns.is_some()
                || params.end_ts_ns.is_some(),
            params.min_dur_ms.is_some(),
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome main-thread hotspots").await?;
        let effective_limit = bounded_tool_limit(params.limit, DEFAULT_CHROME_TOOL_ROWS)?;
        let probe_limit = extra_row_probe_limit(effective_limit);
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
            limit: Some(probe_limit as u32),
        })
        .map_err(|e| e.to_string())?;
        let table = client
            .query(&sql)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome main-thread hotspots", e))?;
        format_chrome_tool_response_with_probe_limit(table, effective_limit, params.max_string_len)
    }

    #[tool(
        name = "slice_descendants_breakdown",
        description = "Recursive child-slice expansion under known `slice.id` roots, \
                       aggregated as a bounded breakdown per (depth, name) group. \
                       Use to drill into a long task — after `chrome_main_thread_hotspots` \
                       or `execute_sql` returns a slice id — without hand-writing \
                       `WITH RECURSIVE` CTEs over `slice.parent_id`. Required: \
                       `slice_ids`. Optional bounds: `min_dur_ms`, `max_depth`, \
                       `limit`, `include_args`, `max_string_len`. The response echoes \
                       `summary_scope`, `applied_filters`, and `missing_root_ids` \
                       (missing root slice ids). Returned columns: `root_id`, `depth`, `name`, \
                       `slice_count`, `inclusive_total_ms` (do not sum across depths), \
                       `self_ms` (direct-child time subtracted, clamped at zero), \
                       `max_ms`, `first_ts_ns` (raw \
                       nanoseconds, not ms), `example_slice_id` (longest-duration \
                       descendant per group), and optionally `example_args`. \
                       `incomplete_descendant_count` counts dur<0 descendants excluded \
                       from duration aggregates.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "slice_descendants_breakdown", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn slice_descendants_breakdown(
        &self,
        Parameters(params): Parameters<SliceDescendantsBreakdownParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "slice_id_count={},min_dur_ms_set={},max_depth_set={},limit_set={},include_args={},max_string_len_set={}",
            params.slice_ids.len(),
            params.min_dur_ms.is_some(),
            params.max_depth.is_some(),
            params.limit.is_some(),
            params.include_args,
            params.max_string_len.is_some()
        ))
        .await;
        let max_string_len = tool_max_string_len(params.max_string_len)?;
        let effective_limit =
            slice_descendants_effective_limit(params.limit).map_err(|e| e.to_string())?;
        let probe_limit = extra_row_probe_limit(effective_limit as usize) as u32;
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
        let incomplete_descendant_count = fetch_incomplete_slice_descendant_count(
            &client,
            &deduped_root_ids,
            applied_filters.max_depth,
        )
        .await
        .map_err(format_slice_descendants_tool_error)?;

        let sql = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
            slice_ids: &params.slice_ids,
            min_dur_ms: params.min_dur_ms,
            max_depth: params.max_depth,
            include_args: params.include_args,
            row_limit: probe_limit,
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
            incomplete_descendant_count,
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
                       Parameters: optional `limit` (default 100, capped at 5000) and \
                       `max_string_len`. Operates on the loaded trace.\n\
                       \n\
                       Output: metadata-first JSON; `row_count` exact; \
                       `truncated=true` means more rows exist; \
                       `string_truncated=true` means shortened text.\n\
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_startup_summary", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_startup_summary(
        &self,
        Parameters(params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "limit_set={},max_string_len_set={}",
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let effective_limit = bounded_tool_limit(params.limit, DEFAULT_CHROME_TOOL_ROWS)?;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome startup summary").await?;
        let table = client
            .query(&chrome_startup_summary_sql(effective_limit))
            .await
            .map_err(|e| format_chrome_tool_error("Chrome startup summary", e))?;
        let count_table = client
            .query(CHROME_STARTUP_SUMMARY_COUNT_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome startup summary count", e))?;
        let row_count = decoded_row_count(&count_table, "Chrome startup summary")?;
        format_chrome_tool_response_with_known_row_count(table, row_count, params.max_string_len)
    }

    #[tool(
        name = "chrome_web_content_interactions",
        description = "Rank Chrome web content interactions by total_duration_ms: \
                       id, ts, total_duration_ms, longest_event_dur_ms, \
                       interaction_type, renderer_upid. Read-only.\n\
                       \n\
                       Use when: INP analysis, reproducing user-felt latency, finding \
                       slow click/tap/keyboard handlers.\n\
                       \n\
                       Don't use for: non-Chrome traces (will error). For interactions \
                       filtered by `interaction_type`, drop to \
                       `execute_sql` against `chrome.web_content_interactions`.\n\
                       \n\
                       Parameters: optional `limit` (default 100, capped at 5000) and \
                       `max_string_len`. Operates on the loaded trace.\n\
                       \n\
                       Output: metadata-first JSON; `row_count` exact; \
                       `truncated=true` means more rows exist; \
                       `string_truncated=true` means shortened text.\n\
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "chrome_web_content_interactions", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn chrome_web_content_interactions(
        &self,
        Parameters(params): Parameters<ChromeTraceParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "limit_set={},max_string_len_set={}",
            params.limit.is_some(),
            params.max_string_len.is_some()
        ))
        .await;
        let effective_limit = bounded_tool_limit(params.limit, DEFAULT_CHROME_TOOL_ROWS)?;
        let client = self.client_for_current().await?;
        ensure_chrome_trace(&client, "Chrome web content interactions").await?;
        let table = client
            .query(&chrome_web_content_interactions_sql(effective_limit))
            .await
            .map_err(|e| format_chrome_tool_error("Chrome web content interactions", e))?;
        let count_table = client
            .query(CHROME_WEB_CONTENT_INTERACTIONS_COUNT_SQL)
            .await
            .map_err(|e| format_chrome_tool_error("Chrome web content interactions count", e))?;
        let row_count = decoded_row_count(&count_table, "Chrome web content interactions")?;
        format_chrome_tool_response_with_known_row_count(table, row_count, params.max_string_len)
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
    #[tracing::instrument(
        level = "debug",
        name = "mcp.tool",
        skip_all,
        fields(tool = "list_stdlib_modules", trace_loaded = tracing::field::Empty, param_summary = tracing::field::Empty)
    )]
    async fn list_stdlib_modules(
        &self,
        Parameters(params): Parameters<ListStdlibModulesParams>,
    ) -> Result<String, String> {
        self.record_tool_span(format!(
            "domain_set={},query_set={},limit_set={}",
            params.domain.is_some(),
            params.query.is_some(),
            params.limit.is_some()
        ))
        .await;
        filtered_stdlib_modules_json(&params)
    }
}

impl PerfettoMcpServer {
    pub fn new(manager: Arc<TraceProcessorManager>) -> Self {
        Self {
            manager,
            current_trace: Arc::new(Mutex::new(None)),
            schema_cache: Arc::new(Mutex::new(SchemaCache::default())),
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

    async fn record_tool_span(&self, param_summary: String) {
        let trace_loaded = self.current_trace.lock().await.is_some();
        let span = tracing::Span::current();
        span.record("trace_loaded", trace_loaded);
        span.record("param_summary", param_summary);
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

async fn table_has_column(
    client: &TraceProcessorClient,
    table_name: &str,
    column_name: &str,
) -> Result<bool, String> {
    let sql = format!("PRAGMA table_info('{table_name}')");
    let table = client
        .query(&sql)
        .await
        .map_err(|e| format!("Failed to inspect table {table_name:?}: {e}"))?;
    Ok(decoded_table_has_column(&table, column_name))
}

fn decoded_table_has_column(table: &DecodedTable, column_name: &str) -> bool {
    (0..table.len())
        .any(|i| table.cell(i, "name").and_then(|value| value.as_str()) == Some(column_name))
}

fn reject_table_structure_pattern(table_name: &str) -> Result<(), String> {
    if table_name.contains('*') || table_name.contains('?') {
        return Err(
            "`list_table_structure` requires one exact table/view name; use `list_tables` with \
             `pattern` first, then pass one returned name."
                .to_owned(),
        );
    }
    Ok(())
}

fn trace_schema_cache_key(trace_path: &str) -> Result<SchemaCacheTraceKey, String> {
    let path = Path::new(trace_path);
    let canonical_path = path
        .canonicalize()
        .map_err(|e| format!("Failed to stat current trace {trace_path:?}: {e}"))?;
    let metadata = std::fs::metadata(&canonical_path)
        .map_err(|e| format!("Failed to stat current trace {:?}: {e}", canonical_path))?;
    let sample_sha256 = trace_file_sample_sha256(&canonical_path, metadata.len()).map_err(|e| {
        format!(
            "Failed to fingerprint current trace {:?}: {e}",
            canonical_path
        )
    })?;
    Ok(SchemaCacheTraceKey {
        canonical_path,
        size_bytes: metadata.len(),
        modified: metadata.modified().ok(),
        platform: trace_file_platform_fingerprint(&metadata),
        sample_sha256,
    })
}

#[cfg(test)]
mod tests;
