// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use crate::error::{PerfettoError, QueryErrorKind};

pub(crate) fn sql_span_kind(sql: &str) -> &'static str {
    let normalized = sql.trim_start();
    let lower = normalized.to_ascii_lowercase();

    if lower.contains("flat_key = 'chrome.process_type'") {
        return "chrome_preflight";
    }
    if lower.contains("from metadata") {
        return "load_trace_metadata";
    }
    if lower.contains("trace_start()") || lower.contains("trace_dur()") {
        return "load_trace_overview";
    }
    if lower.starts_with("select id from slice where id in") {
        return "slice_missing_roots";
    }
    if lower.starts_with("pragma ") {
        return "schema_pragma";
    }
    if lower.contains("chrome.scroll_jank.scroll_jank_v3") {
        return "chrome_scroll_jank";
    }
    if lower.contains("chrome.web_content_interactions") {
        return "chrome_web_interactions";
    }
    if lower.contains("chrome.tasks") {
        return "chrome_tasks";
    }
    if lower.contains("chrome.page_loads") {
        return "chrome_page_loads";
    }
    if lower.contains("chrome.startups") {
        return "chrome_startups";
    }
    if lower.starts_with("with recursive") {
        return "recursive_query";
    }
    if lower.starts_with("select ") || lower.starts_with("with ") {
        return "custom_select";
    }

    "custom"
}

pub(crate) fn perfetto_error_span_kind(err: &PerfettoError) -> &'static str {
    match err {
        PerfettoError::QueryError { kind, .. } => match kind {
            QueryErrorKind::MissingTable => "query_missing_table",
            QueryErrorKind::MissingModule => "query_missing_module",
            QueryErrorKind::MissingColumn => "query_missing_column",
            QueryErrorKind::MultipleOutputStatements => "query_multiple_output_statements",
            QueryErrorKind::Other => "query_other",
        },
        PerfettoError::TooManyRows => "too_many_rows",
        PerfettoError::RpcError(_) => "rpc",
        PerfettoError::DecodeError(_) => "decode",
        PerfettoError::InvalidParam(_) => "invalid_param",
        PerfettoError::Other(_) => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_span_kind_classifies_internal_probe_queries() {
        assert_eq!(
            sql_span_kind(
                "SELECT EXISTS(SELECT 1 FROM args WHERE flat_key = 'chrome.process_type') AS n"
            ),
            "chrome_preflight"
        );
        assert_eq!(
            sql_span_kind("SELECT name, str_value, int_value FROM metadata ORDER BY name"),
            "load_trace_metadata"
        );
        assert_eq!(
            sql_span_kind("SELECT trace_start() AS start_ts, trace_end() AS end_ts"),
            "load_trace_overview"
        );
        assert_eq!(
            sql_span_kind("SELECT id FROM slice WHERE id IN (1, 2, 3)"),
            "slice_missing_roots"
        );
    }

    #[test]
    fn sql_span_kind_classifies_chrome_stdlib_queries() {
        assert_eq!(
            sql_span_kind("INCLUDE PERFETTO MODULE chrome.tasks; SELECT * FROM chrome_tasks"),
            "chrome_tasks"
        );
        assert_eq!(
            sql_span_kind(
                "INCLUDE PERFETTO MODULE chrome.page_loads; SELECT * FROM chrome_page_loads"
            ),
            "chrome_page_loads"
        );
        assert_eq!(
            sql_span_kind("INCLUDE PERFETTO MODULE chrome.scroll_jank.scroll_jank_v3; SELECT * FROM chrome_janky_frames"),
            "chrome_scroll_jank"
        );
        assert_eq!(
            sql_span_kind("INCLUDE PERFETTO MODULE chrome.web_content_interactions; SELECT * FROM chrome_web_content_interactions"),
            "chrome_web_interactions"
        );
    }

    #[test]
    fn sql_span_kind_uses_coarse_fallbacks() {
        assert_eq!(
            sql_span_kind("WITH RECURSIVE roots(root_id) AS (VALUES (1)) SELECT * FROM roots"),
            "recursive_query"
        );
        assert_eq!(
            sql_span_kind("SELECT * FROM process LIMIT 1"),
            "custom_select"
        );
        assert_eq!(sql_span_kind("PRAGMA table_info('slice')"), "schema_pragma");
        assert_eq!(sql_span_kind("garbage"), "custom");
    }
}
