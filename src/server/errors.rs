use crate::error::{PerfettoError, QueryErrorKind, MAX_ROWS};
/// Hints are kind-gated so unrelated SQL errors don't get misrouted. The
/// MissingColumn hint is intentionally view-agnostic — naming specific
/// stdlib views (e.g. only `chrome_page_loads`) would bias recovery for
/// queries against `slice` / `args` / `thread_state` etc., so the hint
/// names both the stdlib path (`INCLUDE PERFETTO MODULE`) and base tables
/// without favoring either.
pub(super) fn format_execute_sql_error(err: PerfettoError) -> String {
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
pub(super) fn format_chrome_tool_error(tool_label: &str, err: PerfettoError) -> String {
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

pub(super) fn format_slice_descendants_tool_error(err: PerfettoError) -> String {
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
