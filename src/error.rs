// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

/// Maximum number of rows returned from a single query.
pub const MAX_ROWS: usize = 5000;

/// Coarse classification of a `trace_processor_shell` query error.
/// Classified once at the decode boundary so consumers match on a stable
/// enum instead of substring-checking upstream wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum QueryErrorKind {
    MissingTable,
    MissingModule,
    MissingColumn,
    MultipleOutputStatements,
    Other,
}

impl QueryErrorKind {
    /// Bucket a raw `trace_processor_shell` error message into a
    /// `QueryErrorKind`. SQLite emits `"no such table: ..."` and
    /// `"no such column: ..."` in lower case. Perfetto stdlib module wording
    /// has drifted across versions, so keep both the older and v54.0 forms.
    pub(crate) fn classify(message: &str) -> Self {
        if message.contains("no such table:") {
            QueryErrorKind::MissingTable
        } else if message.contains("Module not found:")
            || message.contains("INCLUDE: unknown module")
        {
            QueryErrorKind::MissingModule
        } else if message.contains("no such column:") {
            QueryErrorKind::MissingColumn
        } else if message.contains("Result rows were returned for multiples queries")
            || message.contains("multiple output statements")
        {
            QueryErrorKind::MultipleOutputStatements
        } else {
            QueryErrorKind::Other
        }
    }
}

#[derive(Debug, Error)]
pub enum PerfettoError {
    #[error("query error: {message}")]
    QueryError {
        kind: QueryErrorKind,
        message: String,
    },

    #[error("query exceeded {MAX_ROWS} row limit")]
    TooManyRows,

    #[error("RPC error: {0}")]
    RpcError(#[from] reqwest::Error),

    #[error("protobuf decode error: {0}")]
    DecodeError(#[from] prost::DecodeError),

    #[error("invalid parameter: {0}")]
    InvalidParam(String),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_recognizes_missing_table() {
        assert_eq!(
            QueryErrorKind::classify("no such table: foo"),
            QueryErrorKind::MissingTable,
        );
    }

    #[test]
    fn classify_recognizes_missing_module() {
        assert_eq!(
            QueryErrorKind::classify("Module not found: chrome.scroll_jank.scroll_jank_v3"),
            QueryErrorKind::MissingModule,
        );
    }

    #[test]
    fn classify_recognizes_v54_missing_module() {
        assert_eq!(
            QueryErrorKind::classify("INCLUDE: unknown module 'chrome.page_load'"),
            QueryErrorKind::MissingModule,
        );
    }

    #[test]
    fn classify_recognizes_missing_column() {
        assert_eq!(
            QueryErrorKind::classify("no such column: navigation_id"),
            QueryErrorKind::MissingColumn,
        );
    }

    #[test]
    fn classify_recognizes_multiple_output_statements() {
        assert_eq!(
            QueryErrorKind::classify("multiple output statements are not supported"),
            QueryErrorKind::MultipleOutputStatements,
        );
    }

    #[test]
    fn classify_falls_back_to_other_for_unrelated_errors() {
        assert_eq!(
            QueryErrorKind::classify("syntax error near WHERE"),
            QueryErrorKind::Other,
        );
    }

    #[test]
    fn classify_does_not_misroute_status_failure_text() {
        assert_eq!(
            QueryErrorKind::classify("simulated transient /status failure"),
            QueryErrorKind::Other,
        );
    }
}
