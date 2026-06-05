use serde::Serialize;

use crate::error::{PerfettoError, MAX_ROWS};
use crate::params::{
    default_redact_strings, ExecuteSqlParams, SliceDescendantsBreakdownParams,
    REDACT_STRINGS_DEFAULT_ENV,
};
use crate::query::{DecodeQueryOptions, DecodedQueryResult, DecodedTable};
use crate::sql_templates::{
    dedupe_preserving_order, DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
    DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
};
use crate::stdlib_catalog::STDLIB_INSTRUCTIONS;
pub(super) const DEFAULT_CHROME_TOOL_ROWS: usize = 100;
pub(super) const DEFAULT_TOOL_MAX_STRING_LEN: Option<usize> = None;
pub(super) const DEFAULT_EXECUTE_SQL_SUMMARY_ROWS: usize = 10;
pub(super) const EXECUTE_SQL_SHAPING_NOTE: &str =
    "row_count is exact post-SQL decoded rows when row_count_known=true; \
     head/limit only trims returned tool rows and does not rewrite SQL.";
pub(super) const CHROME_TOOL_SHAPING_NOTE: &str =
    "row_count unknown; truncated=row cap reached; string_truncated=cell text shortened.";
pub(super) const SLICE_DESCENDANTS_BREAKDOWN_SCOPE: &str = "descendants only; root slices excluded";
pub(super) const SLICE_DESCENDANTS_SHAPING_NOTE: &str =
    "slice_count and total_ms include only descendants matching min_dur_ms within max_depth; \
     limit caps returned groups; example_slice_id is the longest-duration descendant per group \
     (ties broken by smallest id); first_ts_ns is raw nanoseconds; missing_root_ids lists \
     requested slice_ids that do not exist in the loaded trace; example_args, when present, \
     comes only from example_slice_id.";
pub(super) const REDACTION_POLICY_NOTE: &str =
    "execute_sql and Chrome dedicated-tool string cells may contain <redacted>; this is server-side policy, not a tool parameter.";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(super) struct RedactionPolicy {
    pub(super) execute_sql_string_cells: bool,
    pub(super) chrome_tool_string_cells: bool,
    pub(super) env_var: &'static str,
    pub(super) note: &'static str,
}

pub(super) fn redaction_policy_for(enabled: bool) -> RedactionPolicy {
    RedactionPolicy {
        execute_sql_string_cells: enabled,
        chrome_tool_string_cells: enabled,
        env_var: REDACT_STRINGS_DEFAULT_ENV,
        note: REDACTION_POLICY_NOTE,
    }
}

pub(super) fn current_redaction_policy() -> RedactionPolicy {
    redaction_policy_for(default_redact_strings())
}

pub(super) fn server_instructions() -> String {
    server_instructions_for_redaction(default_redact_strings())
}

pub(super) fn server_instructions_for_redaction(enabled: bool) -> String {
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
pub(super) enum ExecuteSqlOutputMode {
    FullRows,
    LimitedRows(usize),
    Summary(usize),
    ColumnsOnly,
}

impl ExecuteSqlOutputMode {
    fn as_str(self) -> &'static str {
        match self {
            ExecuteSqlOutputMode::FullRows => "full_rows",
            ExecuteSqlOutputMode::LimitedRows(_) => "limited_rows",
            ExecuteSqlOutputMode::Summary(_) => "summary",
            ExecuteSqlOutputMode::ColumnsOnly => "columns_only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExecuteSqlOutputShape {
    pub(super) mode: ExecuteSqlOutputMode,
    pub(super) active: bool,
    pub(super) max_string_len: Option<usize>,
    pub(super) redact_strings: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct ExecuteSqlRowsResponse {
    pub(super) columns: Vec<String>,
    pub(super) row_count: usize,
    pub(super) returned_rows: usize,
    pub(super) truncated: bool,
    pub(super) row_count_known: bool,
    pub(super) string_truncated: bool,
    pub(super) redacted: bool,
    pub(super) note: &'static str,
    pub(super) rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub(super) struct ExecuteSqlSummaryResponse {
    pub(super) columns: Vec<String>,
    pub(super) row_count: usize,
    pub(super) returned_rows: usize,
    pub(super) truncated: bool,
    pub(super) row_count_known: bool,
    pub(super) string_truncated: bool,
    pub(super) redacted: bool,
    pub(super) note: &'static str,
    pub(super) sample_rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub(super) struct ExecuteSqlColumnsOnlyResponse {
    pub(super) columns: Vec<String>,
    pub(super) row_count: usize,
    pub(super) returned_rows: usize,
    pub(super) truncated: bool,
    pub(super) row_count_known: bool,
    pub(super) string_truncated: bool,
    pub(super) redacted: bool,
    pub(super) note: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct ChromeToolRowsResponse {
    pub(super) columns: Vec<String>,
    pub(super) row_count: Option<usize>,
    pub(super) returned_rows: usize,
    pub(super) truncated: bool,
    pub(super) row_count_known: bool,
    pub(super) string_truncated: bool,
    pub(super) redacted: bool,
    pub(super) note: &'static str,
    pub(super) rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(super) struct ChromeResourceTimingEvidence {
    pub(super) attribution_scope: &'static str,
    pub(super) phase_breakdown: &'static str,
    pub(super) phase_breakdown_available: bool,
    pub(super) safe_conclusion: &'static str,
    pub(super) safe_fact_fields: Vec<&'static str>,
    pub(super) unsafe_inferences: Vec<&'static str>,
    pub(super) hypothesis_only: Vec<&'static str>,
    pub(super) network_phase_slice_count: i64,
    pub(super) network_phase_arg_count: i64,
    pub(super) incomplete_resource_slice_count: i64,
    pub(super) incomplete_slices_excluded: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct ChromeResourceSummaryRowsResponse {
    pub(super) columns: Vec<String>,
    pub(super) row_count: Option<usize>,
    pub(super) returned_rows: usize,
    pub(super) truncated: bool,
    pub(super) row_count_known: bool,
    pub(super) string_truncated: bool,
    pub(super) redacted: bool,
    pub(super) resource_timing_evidence: ChromeResourceTimingEvidence,
    pub(super) note: &'static str,
    pub(super) rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(super) struct SliceDescendantsAppliedFilters {
    pub(super) min_dur_ms: f64,
    pub(super) max_depth: u32,
    pub(super) limit: u32,
    pub(super) include_args: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct SliceDescendantsRowsResponse {
    pub(super) columns: Vec<String>,
    pub(super) row_count: Option<usize>,
    pub(super) returned_rows: usize,
    pub(super) truncated: bool,
    pub(super) row_count_known: bool,
    pub(super) string_truncated: bool,
    pub(super) redacted: bool,
    pub(super) summary_scope: &'static str,
    pub(super) applied_filters: SliceDescendantsAppliedFilters,
    /// Requested `slice_ids` that do not exist in the loaded trace's `slice`
    /// table. Always present in the response (empty when all roots existed)
    /// so LLM callers can tell "no descendants" apart from "stale id".
    pub(super) missing_root_ids: Vec<i64>,
    pub(super) note: &'static str,
    pub(super) rows: Vec<Vec<serde_json::Value>>,
}

pub(super) fn chrome_hotspots_effective_limit(limit: Option<u32>) -> usize {
    chrome_hotspots_effective_limit_with_default(limit, DEFAULT_CHROME_TOOL_ROWS)
}

pub(super) fn chrome_hotspots_effective_limit_with_default(
    limit: Option<u32>,
    default_limit: usize,
) -> usize {
    match limit {
        Some(n) if (n as usize) > MAX_ROWS => MAX_ROWS,
        Some(n) => n as usize,
        None => default_limit,
    }
}

pub(super) fn tool_max_string_len(max_string_len: Option<u32>) -> Result<Option<usize>, String> {
    match max_string_len {
        Some(0) => Err("`max_string_len` must be > 0 when set.".to_owned()),
        Some(n) => Ok(Some(n as usize)),
        None => Ok(DEFAULT_TOOL_MAX_STRING_LEN),
    }
}

pub(super) fn format_chrome_tool_response(
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

pub(super) fn format_chrome_tool_response_with_redaction(
    table: DecodedTable,
    effective_limit: usize,
    max_string_len: Option<usize>,
    redact_strings: bool,
) -> Result<String, String> {
    let span = tracing::debug_span!(
        "mcp.response_shape",
        response = "chrome_rows",
        row_count = table.rows.len(),
        effective_limit,
        max_string_len_set = max_string_len.is_some(),
        redact_strings,
        string_truncated = tracing::field::Empty,
        redacted = tracing::field::Empty,
    );
    let _entered = span.enter();
    let shape = ExecuteSqlOutputShape {
        mode: ExecuteSqlOutputMode::FullRows,
        active: true,
        max_string_len,
        redact_strings,
    };
    let returned_rows = table.rows.len();
    let (rows, string_truncated, redacted) = transform_rows(table.rows.iter(), shape);
    tracing::Span::current().record("string_truncated", string_truncated);
    tracing::Span::current().record("redacted", redacted);
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

pub(super) fn format_chrome_resource_summary_response(
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

pub(super) fn format_chrome_resource_summary_response_with_redaction(
    table: DecodedTable,
    effective_limit: usize,
    max_string_len: Option<usize>,
    redact_strings: bool,
    evidence: ChromeResourceTimingEvidence,
) -> Result<String, String> {
    let span = tracing::debug_span!(
        "mcp.response_shape",
        response = "chrome_resource_summary",
        row_count = table.rows.len(),
        effective_limit,
        max_string_len_set = max_string_len.is_some(),
        redact_strings,
        phase_breakdown_available = evidence.phase_breakdown_available,
        string_truncated = tracing::field::Empty,
        redacted = tracing::field::Empty,
    );
    let _entered = span.enter();
    let shape = ExecuteSqlOutputShape {
        mode: ExecuteSqlOutputMode::FullRows,
        active: true,
        max_string_len,
        redact_strings,
    };
    let returned_rows = table.rows.len();
    let (rows, string_truncated, redacted) = transform_rows(table.rows.iter(), shape);
    tracing::Span::current().record("string_truncated", string_truncated);
    tracing::Span::current().record("redacted", redacted);
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

pub(super) fn chrome_resource_timing_evidence_from_probe(
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

pub(super) fn decoded_table_i64_cell(table: &DecodedTable, col: &str) -> Option<i64> {
    table.cell(0, col).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
    })
}

pub(super) fn slice_descendants_applied_filters(
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

pub(super) fn format_slice_descendants_tool_response_with_redaction(
    table: DecodedTable,
    effective_limit: usize,
    applied_filters: SliceDescendantsAppliedFilters,
    missing_root_ids: Vec<i64>,
    max_string_len: Option<usize>,
    redact_strings: bool,
) -> Result<String, String> {
    let span = tracing::debug_span!(
        "mcp.response_shape",
        response = "slice_descendants",
        row_count = table.rows.len(),
        effective_limit,
        missing_root_count = missing_root_ids.len(),
        max_string_len_set = max_string_len.is_some(),
        redact_strings,
        string_truncated = tracing::field::Empty,
        redacted = tracing::field::Empty,
    );
    let _entered = span.enter();
    let shape = ExecuteSqlOutputShape {
        mode: ExecuteSqlOutputMode::FullRows,
        active: true,
        max_string_len,
        redact_strings,
    };
    let returned_rows = table.rows.len();
    let (rows, string_truncated, redacted) = transform_rows(table.rows.iter(), shape);
    tracing::Span::current().record("string_truncated", string_truncated);
    tracing::Span::current().record("redacted", redacted);
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
pub(super) fn dedupe_slice_ids_preserving_order(ids: &[i64]) -> Vec<i64> {
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
#[tracing::instrument(
    level = "debug",
    name = "slice_descendants.missing_roots",
    skip(client, deduped_root_ids),
    fields(root_count = deduped_root_ids.len(), missing_root_count = tracing::field::Empty)
)]
pub(super) async fn fetch_missing_slice_ids(
    client: &crate::tp_client::TraceProcessorClient,
    deduped_root_ids: &[i64],
) -> Result<Vec<i64>, PerfettoError> {
    if deduped_root_ids.is_empty() {
        tracing::Span::current().record("missing_root_count", 0);
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
    let missing = deduped_root_ids
        .iter()
        .copied()
        .filter(|id| !found.contains(id))
        .collect::<Vec<_>>();
    tracing::Span::current().record("missing_root_count", missing.len());
    Ok(missing)
}

#[cfg(test)]
pub(super) fn format_execute_sql_response_with_redaction(
    table: DecodedTable,
    params: &ExecuteSqlParams,
    redact_strings: bool,
) -> Result<String, String> {
    let row_count = table.rows.len();
    format_execute_sql_decoded_response_with_redaction(
        DecodedQueryResult {
            table,
            row_count,
            row_count_known: true,
            rows_truncated: false,
        },
        params,
        redact_strings,
    )
}

pub(super) fn format_execute_sql_decoded_response_with_redaction(
    decoded: DecodedQueryResult,
    params: &ExecuteSqlParams,
    redact_strings: bool,
) -> Result<String, String> {
    let shape = execute_sql_output_shape(params, redact_strings)?;
    let table = decoded.table;
    let span = tracing::debug_span!(
        "mcp.response_shape",
        response = "execute_sql",
        row_count = table.rows.len(),
        decoded_row_count = decoded.row_count,
        row_count_known = decoded.row_count_known,
        rows_truncated = decoded.rows_truncated,
        shape_active = shape.active,
        output_mode = shape.mode.as_str(),
        max_string_len_set = shape.max_string_len.is_some(),
        redact_strings,
        string_truncated = tracing::field::Empty,
        redacted = tracing::field::Empty,
    );
    let _entered = span.enter();
    if !shape.active {
        tracing::Span::current().record("string_truncated", false);
        tracing::Span::current().record("redacted", false);
        return serde_json::to_string(&table)
            .map_err(|e| format!("Failed to serialize results: {e}"));
    }

    let row_count = decoded.row_count;
    match shape.mode {
        ExecuteSqlOutputMode::ColumnsOnly => {
            tracing::Span::current().record("string_truncated", false);
            tracing::Span::current().record("redacted", false);
            serde_json::to_string(&ExecuteSqlColumnsOnlyResponse {
                columns: table.columns,
                row_count,
                returned_rows: 0,
                truncated: false,
                row_count_known: decoded.row_count_known,
                string_truncated: false,
                redacted: false,
                note: EXECUTE_SQL_SHAPING_NOTE,
            })
            .map_err(|e| format!("Failed to serialize results: {e}"))
        }
        ExecuteSqlOutputMode::Summary(limit) => {
            let (sample_rows, string_truncated, redacted) =
                transform_rows(table.rows.iter().take(limit), shape);
            tracing::Span::current().record("string_truncated", string_truncated);
            tracing::Span::current().record("redacted", redacted);
            serde_json::to_string(&ExecuteSqlSummaryResponse {
                columns: table.columns,
                returned_rows: sample_rows.len(),
                sample_rows,
                row_count,
                truncated: decoded.rows_truncated || row_count > limit,
                row_count_known: decoded.row_count_known,
                string_truncated,
                redacted,
                note: EXECUTE_SQL_SHAPING_NOTE,
            })
            .map_err(|e| format!("Failed to serialize results: {e}"))
        }
        ExecuteSqlOutputMode::LimitedRows(limit) => {
            let (rows, string_truncated, redacted) =
                transform_rows(table.rows.iter().take(limit), shape);
            tracing::Span::current().record("string_truncated", string_truncated);
            tracing::Span::current().record("redacted", redacted);
            serde_json::to_string(&ExecuteSqlRowsResponse {
                columns: table.columns,
                returned_rows: rows.len(),
                rows,
                row_count,
                truncated: decoded.rows_truncated || row_count > limit,
                row_count_known: decoded.row_count_known,
                string_truncated,
                redacted,
                note: EXECUTE_SQL_SHAPING_NOTE,
            })
            .map_err(|e| format!("Failed to serialize results: {e}"))
        }
        ExecuteSqlOutputMode::FullRows => {
            let (rows, string_truncated, redacted) = transform_rows(table.rows.iter(), shape);
            tracing::Span::current().record("string_truncated", string_truncated);
            tracing::Span::current().record("redacted", redacted);
            serde_json::to_string(&ExecuteSqlRowsResponse {
                columns: table.columns,
                returned_rows: rows.len(),
                rows,
                row_count,
                truncated: false,
                row_count_known: decoded.row_count_known,
                string_truncated,
                redacted,
                note: EXECUTE_SQL_SHAPING_NOTE,
            })
            .map_err(|e| format!("Failed to serialize results: {e}"))
        }
    }
}

pub(super) fn execute_sql_output_shape(
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

pub(super) fn execute_sql_decode_options(
    params: &ExecuteSqlParams,
) -> Result<DecodeQueryOptions, String> {
    let shape = execute_sql_output_shape(params, false)?;
    let max_rows = match shape.mode {
        ExecuteSqlOutputMode::ColumnsOnly => Some(0),
        ExecuteSqlOutputMode::Summary(limit) | ExecuteSqlOutputMode::LimitedRows(limit) => {
            Some(limit)
        }
        ExecuteSqlOutputMode::FullRows => None,
    };
    Ok(DecodeQueryOptions { max_rows })
}

pub(super) fn transform_rows<'a>(
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

pub(super) fn transform_value(
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

pub(super) fn truncate_string_cell(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_owned(), false);
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("...<truncated>");
    (out, true)
}

pub(super) fn redact_string_cell(s: &str) -> (String, bool) {
    let (s, redacted_headers) = redact_sensitive_header_lines(s);
    let (s, redacted_paths) = redact_user_path_segments(&s);
    let (s, redacted_assignments) = redact_sensitive_assignments(&s);
    (
        s,
        redacted_headers || redacted_paths || redacted_assignments,
    )
}

pub(super) fn redact_sensitive_header_lines(s: &str) -> (String, bool) {
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

pub(super) fn redact_user_path_segments(s: &str) -> (String, bool) {
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

pub(super) fn redact_sensitive_assignments(s: &str) -> (String, bool) {
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

pub(super) fn redact_sensitive_assignment_marker(
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

pub(super) fn has_sensitive_key_boundary(s: &str, key_start: usize, encoded_marker: bool) -> bool {
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

pub(super) fn has_ascii_suffix_ignore_case(s: &str, suffix: &str) -> bool {
    let bytes = s.as_bytes();
    let suffix = suffix.as_bytes();
    bytes.len() >= suffix.len() && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

pub(super) fn sensitive_assignment_value_end(
    s: &str,
    value_start: usize,
    encoded_marker: bool,
) -> usize {
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
