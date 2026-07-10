// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use tracing::Instrument;

use crate::error::PerfettoError;
use crate::proto::{QueryArgs, QueryResult, StatusResult};
use crate::query::{
    decode_query_result_with_options, DecodeQueryOptions, DecodedQueryResult, DecodedTable,
};
use crate::telemetry::{perfetto_error_span_kind, sql_span_kind};

/// HTTP client for a single trace_processor_shell RPC instance.
#[derive(Clone)]
pub struct TraceProcessorClient {
    base_url: String,
    http: reqwest::Client,
    _instance_lease: Option<Arc<dyn Any + Send + Sync>>,
}

impl fmt::Debug for TraceProcessorClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceProcessorClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl TraceProcessorClient {
    /// Create a client targeting `http://localhost:{port}`.
    pub fn new(port: u16, request_timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            .expect("failed to build HTTP client");
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            http,
            _instance_lease: None,
        }
    }

    pub(crate) fn with_instance_lease<T>(mut self, lease: Arc<T>) -> Self
    where
        T: Any + Send + Sync,
    {
        self._instance_lease = Some(lease);
        self
    }

    /// Execute a SQL query and return the decoded columnar table.
    pub async fn query(&self, sql: &str) -> Result<DecodedTable, PerfettoError> {
        Ok(self
            .query_with_options(sql, DecodeQueryOptions::default())
            .await?
            .table)
    }

    /// Execute a SQL query and return decoded rows plus completeness metadata.
    pub async fn query_with_options(
        &self,
        sql: &str,
        options: DecodeQueryOptions,
    ) -> Result<DecodedQueryResult, PerfettoError> {
        let span = tracing::debug_span!(
            "trace_processor.query",
            rpc = "/query",
            sql_kind = sql_span_kind(sql),
            sql_len = sql.len(),
            decode_limited = options.max_rows.is_some(),
            decode_max_rows = options.max_rows.unwrap_or(0),
            row_count = tracing::field::Empty,
            returned_rows = tracing::field::Empty,
            rows_truncated = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );

        async move {
            let args = QueryArgs {
                sql_query: Some(sql.to_owned()),
                tag: None,
            };
            let body = args.encode_to_vec();

            let result = async {
                let resp = self
                    .http
                    .post(format!("{}/query", self.base_url))
                    .header("Content-Type", "application/x-protobuf")
                    .body(body)
                    .send()
                    .await?
                    .error_for_status()?;

                let bytes = resp.bytes().await?;
                let result = QueryResult::decode(bytes)?;
                decode_query_result_with_options(&result, options)
            }
            .await;

            match &result {
                Ok(decoded) => {
                    tracing::Span::current().record("row_count", decoded.row_count);
                    tracing::Span::current().record("returned_rows", decoded.table.len());
                    tracing::Span::current().record("rows_truncated", decoded.rows_truncated);
                }
                Err(err) => {
                    tracing::Span::current().record("error_kind", perfetto_error_span_kind(err));
                }
            }

            result
        }
        .instrument(span)
        .await
    }

    /// Get the status of the trace_processor_shell instance.
    pub async fn status(&self) -> Result<StatusResult, PerfettoError> {
        let span = tracing::debug_span!(
            "trace_processor.status",
            rpc = "/status",
            loaded_trace_name_len = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );

        async move {
            let result = async {
                let resp = self
                    .http
                    .get(format!("{}/status", self.base_url))
                    .send()
                    .await?
                    .error_for_status()?;

                let bytes = resp.bytes().await?;
                Ok(StatusResult::decode(bytes)?)
            }
            .await;

            match &result {
                Ok(status) => {
                    let name_len = status.loaded_trace_name.as_ref().map_or(0, Vec::len);
                    tracing::Span::current().record("loaded_trace_name_len", name_len);
                }
                Err(err) => {
                    tracing::Span::current().record("error_kind", perfetto_error_span_kind(err));
                }
            }

            result
        }
        .instrument(span)
        .await
    }
}
