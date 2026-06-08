// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use rmcp::schemars;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::error::{PerfettoError, QueryErrorKind, MAX_ROWS};
use crate::proto::query_result::cells_batch::CellType;
use crate::proto::QueryResult;

/// A decoded SQL result in columnar form.
///
/// `columns` carries the SELECT-clause column names in their original order
/// (sourced directly from `proto.column_names`); each entry of `rows` is one
/// data row whose values align positionally with `columns`. This type is the
/// single boundary for query results — it serializes to the wire shape
/// `{"columns": [...], "rows": [[...], ...]}` and is what every JSON-emitting
/// MCP tool returns inside `rmcp::Json<...>`.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct DecodedTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl DecodedTable {
    /// Number of data rows (not including the column header).
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True iff `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Look up a cell by row index and column name. Returns `None` when
    /// either index is out of range or the column does not exist.
    /// Linear scan over `columns` — fine at the column counts we see
    /// (typically <=20). Avoids a per-instance `HashMap` that would matter
    /// on max-size (5000-row) results.
    pub fn cell(&self, row: usize, col: &str) -> Option<&Value> {
        let idx = self.columns.iter().position(|c| c == col)?;
        self.rows.get(row)?.get(idx)
    }
}

/// Controls how many row values are materialized while decoding a
/// `QueryResult`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeQueryOptions {
    /// Maximum number of rows to materialize. `None` materializes every row
    /// up to `MAX_ROWS`; `Some(0)` counts rows and returns no row values.
    pub max_rows: Option<usize>,
}

/// A decoded query plus completeness metadata for output shaping.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedQueryResult {
    pub table: DecodedTable,
    /// Exact number of complete rows reported by trace_processor for this
    /// output statement. This is counted from cell metadata, so callers can
    /// return accurate `row_count` without materializing every JSON cell.
    pub row_count: usize,
    pub row_count_known: bool,
    /// True when fewer row values were materialized than `row_count`.
    pub rows_truncated: bool,
}

/// Decode a protobuf QueryResult into a columnar `DecodedTable`.
///
/// `columns` is taken straight from `result.column_names` — the
/// SELECT-clause order is preserved, no alphabetization. Each row is a
/// `Vec<Value>` whose entries align positionally with `columns`.
///
/// Returns early with `TooManyRows` if the result exceeds `MAX_ROWS`.
#[cfg(test)]
pub fn decode_query_result(result: &QueryResult) -> Result<DecodedTable, PerfettoError> {
    Ok(decode_query_result_with_options(result, DecodeQueryOptions::default())?.table)
}

pub fn decode_query_result_with_options(
    result: &QueryResult,
    options: DecodeQueryOptions,
) -> Result<DecodedQueryResult, PerfettoError> {
    if let Some(ref err) = result.error {
        if !err.is_empty() {
            return Err(PerfettoError::QueryError {
                kind: QueryErrorKind::classify(err),
                message: err.clone(),
            });
        }
    }

    if result.statement_with_output_count.unwrap_or(0) > 1 {
        let output_count = result.statement_with_output_count.unwrap_or(0);
        let statement_count = result.statement_count.unwrap_or(0);
        return Err(PerfettoError::QueryError {
            kind: QueryErrorKind::MultipleOutputStatements,
            message: format!(
                "SQL returned rows from {output_count} output statements \
                 (statement_count={statement_count}). execute_sql supports at \
                 most one output-producing statement per call; keep INCLUDE and \
                 setup statements, but make only one SELECT return rows."
            ),
        });
    }

    let columns: Vec<String> = result.column_names.clone();
    let num_cols = columns.len();
    if num_cols == 0 {
        return Ok(DecodedQueryResult {
            table: DecodedTable {
                columns,
                rows: Vec::new(),
            },
            row_count: 0,
            row_count_known: true,
            rows_truncated: false,
        });
    }

    let row_count = complete_row_count(result, num_cols)?;
    if options.max_rows == Some(0) {
        return Ok(DecodedQueryResult {
            table: DecodedTable {
                columns,
                rows: Vec::new(),
            },
            row_count,
            row_count_known: true,
            rows_truncated: row_count > 0,
        });
    }

    let max_rows = options.max_rows.unwrap_or(usize::MAX);
    let mut rows: Vec<Vec<Value>> = Vec::new();

    for batch in &result.batch {
        let mut varint_iter = batch.varint_cells.iter();
        let mut float64_iter = batch.float64_cells.iter();
        let mut blob_iter = batch.blob_cells.iter();
        let mut string_iter = batch.string_cells.as_deref().unwrap_or("").split('\0');

        let mut col_idx: usize = 0;
        let mut current_row: Vec<Value> = Vec::with_capacity(num_cols);

        for &cell_type_raw in &batch.cells {
            let value = match CellType::try_from(cell_type_raw) {
                Ok(CellType::CellNull) | Ok(CellType::CellInvalid) => Value::Null,
                Ok(CellType::CellVarint) => {
                    let v = varint_iter.next().copied().unwrap_or(0);
                    Value::Number(serde_json::Number::from(v))
                }
                Ok(CellType::CellFloat64) => {
                    let v = float64_iter.next().copied().unwrap_or(0.0);
                    float64_cell_to_value(v)
                }
                Ok(CellType::CellString) => {
                    Value::String(string_iter.next().unwrap_or("").to_owned())
                }
                Ok(CellType::CellBlob) => {
                    let b = blob_iter.next().map(Vec::as_slice).unwrap_or(&[]);
                    Value::String(format!("blob:hex:{}", hex::encode(b)))
                }
                Err(_) => Value::Null,
            };

            current_row.push(value);
            col_idx += 1;

            if col_idx == num_cols {
                rows.push(std::mem::replace(
                    &mut current_row,
                    Vec::with_capacity(num_cols),
                ));
                col_idx = 0;

                if rows.len() > MAX_ROWS {
                    return Err(PerfettoError::TooManyRows);
                }
                if rows.len() >= max_rows {
                    return Ok(DecodedQueryResult {
                        table: DecodedTable { columns, rows },
                        row_count,
                        row_count_known: true,
                        rows_truncated: row_count > max_rows,
                    });
                }
            }
        }
    }

    Ok(DecodedQueryResult {
        table: DecodedTable { columns, rows },
        row_count,
        row_count_known: true,
        rows_truncated: false,
    })
}

fn float64_cell_to_value(v: f64) -> Value {
    if let Some(number) = serde_json::Number::from_f64(v) {
        return Value::Number(number);
    }
    if v.is_nan() {
        Value::String("float:NaN".to_owned())
    } else if v.is_sign_negative() {
        Value::String("float:-Infinity".to_owned())
    } else {
        Value::String("float:Infinity".to_owned())
    }
}

fn complete_row_count(result: &QueryResult, num_cols: usize) -> Result<usize, PerfettoError> {
    if num_cols == 0 {
        return Ok(0);
    }
    let mut row_count = 0;
    for (batch_idx, batch) in result.batch.iter().enumerate() {
        let cell_count = batch.cells.len();
        if cell_count % num_cols != 0 {
            return Err(PerfettoError::Other(anyhow::anyhow!(
                "trace_processor returned an incomplete row in batch {batch_idx}: \
                 {cell_count} cells for {num_cols} columns"
            )));
        }
        row_count += cell_count / num_cols;
    }
    Ok(row_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::query_result::CellsBatch;

    fn make_result(columns: Vec<&str>, batches: Vec<CellsBatch>) -> QueryResult {
        QueryResult {
            column_names: columns.into_iter().map(String::from).collect(),
            error: None,
            batch: batches,
            statement_count: None,
            statement_with_output_count: None,
            last_statement_sql: None,
        }
    }

    #[test]
    fn decode_mixed_cell_types() {
        // 3 columns (string, varint, float64) x 2 rows.
        let batch = CellsBatch {
            cells: vec![
                CellType::CellString as i32,
                CellType::CellVarint as i32,
                CellType::CellFloat64 as i32,
                CellType::CellString as i32,
                CellType::CellVarint as i32,
                CellType::CellFloat64 as i32,
            ],
            varint_cells: vec![42, 99],
            float64_cells: vec![1.5, 2.5],
            blob_cells: vec![],
            string_cells: Some("hello\0world".to_owned()),
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["name", "count", "value"], vec![batch]);
        let table = decode_query_result(&result).unwrap();

        assert_eq!(table.columns, vec!["name", "count", "value"]);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(
            table.rows[0],
            vec![Value::from("hello"), Value::from(42), Value::from(1.5)]
        );
        assert_eq!(
            table.rows[1],
            vec![Value::from("world"), Value::from(99), Value::from(2.5)]
        );
    }

    #[test]
    fn decode_non_finite_float64_cells_as_typed_strings() {
        let batch = CellsBatch {
            cells: vec![
                CellType::CellFloat64 as i32,
                CellType::CellFloat64 as i32,
                CellType::CellFloat64 as i32,
                CellType::CellNull as i32,
            ],
            varint_cells: vec![],
            float64_cells: vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY],
            blob_cells: vec![],
            string_cells: None,
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["value"], vec![batch]);
        let table = decode_query_result(&result).unwrap();

        assert_eq!(
            table.rows,
            vec![
                vec![Value::from("float:NaN")],
                vec![Value::from("float:Infinity")],
                vec![Value::from("float:-Infinity")],
                vec![Value::Null],
            ],
            "non-finite floats must not be collapsed into JSON null",
        );
    }

    #[test]
    fn decode_null_cells() {
        let batch = CellsBatch {
            cells: vec![CellType::CellString as i32, CellType::CellNull as i32],
            varint_cells: vec![],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: Some("hello".to_owned()),
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["name", "value"], vec![batch]);
        let table = decode_query_result(&result).unwrap();

        assert_eq!(table.columns, vec!["name", "value"]);
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.rows[0][0], Value::from("hello"));
        assert!(table.rows[0][1].is_null());
    }

    #[test]
    fn decode_empty_result() {
        let result = make_result(vec![], vec![]);
        let table = decode_query_result(&result).unwrap();
        assert!(table.columns.is_empty());
        assert!(table.rows.is_empty());
    }

    #[test]
    fn decode_error_propagated() {
        let result = QueryResult {
            column_names: vec![],
            error: Some("no such table: foo".to_owned()),
            batch: vec![],
            statement_count: None,
            statement_with_output_count: None,
            last_statement_sql: None,
        };
        let err = decode_query_result(&result).unwrap_err();
        assert!(
            matches!(
                err,
                PerfettoError::QueryError {
                    kind: QueryErrorKind::MissingTable,
                    ref message,
                } if message.contains("foo")
            ),
            "expected MissingTable QueryError, got: {err:?}",
        );
    }

    #[test]
    fn decode_rejects_multiple_output_statements() {
        let batch = CellsBatch {
            cells: vec![CellType::CellVarint as i32],
            varint_cells: vec![2],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: None,
            is_last_batch: Some(true),
        };
        let mut result = make_result(vec!["b"], vec![batch]);
        result.statement_count = Some(2);
        result.statement_with_output_count = Some(2);
        result.last_statement_sql = Some("SELECT 2 AS b".to_owned());

        let err = decode_query_result(&result).unwrap_err();
        assert!(
            matches!(
                err,
                PerfettoError::QueryError {
                    kind: QueryErrorKind::MultipleOutputStatements,
                    ref message,
                } if message.contains("at most one output-producing statement")
            ),
            "expected MultipleOutputStatements QueryError, got: {err:?}",
        );
    }

    #[test]
    fn decode_allows_include_plus_single_output_statement() {
        let batch = CellsBatch {
            cells: vec![CellType::CellVarint as i32],
            varint_cells: vec![7],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: None,
            is_last_batch: Some(true),
        };
        let mut result = make_result(vec!["n"], vec![batch]);
        result.statement_count = Some(2);
        result.statement_with_output_count = Some(1);

        let table = decode_query_result(&result).unwrap();
        assert_eq!(table.rows, vec![vec![Value::from(7)]]);
    }

    #[test]
    fn decode_exceeds_row_limit() {
        // Build a batch with MAX_ROWS + 1 rows, 1 column each.
        let row_count = MAX_ROWS + 1;
        let cells = vec![CellType::CellVarint as i32; row_count];
        let varint_cells: Vec<i64> = (0..row_count as i64).collect();
        let batch = CellsBatch {
            cells,
            varint_cells,
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: None,
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["n"], vec![batch]);
        let err = decode_query_result(&result).unwrap_err();
        assert!(
            matches!(err, PerfettoError::TooManyRows),
            "expected TooManyRows, got: {err:?}",
        );
    }

    #[test]
    fn decode_with_row_limit_counts_without_materializing_every_row() {
        let batch = CellsBatch {
            cells: vec![
                CellType::CellVarint as i32,
                CellType::CellString as i32,
                CellType::CellVarint as i32,
                CellType::CellString as i32,
                CellType::CellVarint as i32,
                CellType::CellString as i32,
            ],
            varint_cells: vec![1, 2, 3],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: Some("a\0b\0c".to_owned()),
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["id", "name"], vec![batch]);
        let decoded =
            decode_query_result_with_options(&result, DecodeQueryOptions { max_rows: Some(1) })
                .unwrap();

        assert_eq!(decoded.row_count, 3);
        assert!(decoded.row_count_known);
        assert!(decoded.rows_truncated);
        assert_eq!(
            decoded.table.rows,
            vec![vec![Value::from(1), Value::from("a")]]
        );
    }

    #[test]
    fn decode_blob_cells_as_lossless_hex_with_type_prefix() {
        let batch = CellsBatch {
            cells: vec![CellType::CellBlob as i32, CellType::CellBlob as i32],
            varint_cells: vec![],
            float64_cells: vec![],
            blob_cells: vec![vec![0x00, 0xab, 0xff], Vec::new()],
            string_cells: None,
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["payload"], vec![batch]);
        let table = decode_query_result(&result).unwrap();

        assert_eq!(
            table.rows,
            vec![
                vec![Value::from("blob:hex:00abff")],
                vec![Value::from("blob:hex:")]
            ]
        );
    }

    #[test]
    fn decode_columns_only_counts_rows_without_values() {
        let batch = CellsBatch {
            cells: vec![
                CellType::CellVarint as i32,
                CellType::CellVarint as i32,
                CellType::CellVarint as i32,
            ],
            varint_cells: vec![1, 2, 3],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: None,
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["n"], vec![batch]);
        let decoded =
            decode_query_result_with_options(&result, DecodeQueryOptions { max_rows: Some(0) })
                .unwrap();

        assert_eq!(decoded.row_count, 3);
        assert!(decoded.row_count_known);
        assert!(decoded.rows_truncated);
        assert_eq!(decoded.table.columns, vec!["n"]);
        assert!(decoded.table.rows.is_empty());
    }

    #[test]
    fn decode_rejects_incomplete_row_cells_instead_of_rounding_down_count() {
        let batch = CellsBatch {
            cells: vec![CellType::CellVarint as i32],
            varint_cells: vec![1],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: None,
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["a", "b"], vec![batch]);

        let err =
            decode_query_result_with_options(&result, DecodeQueryOptions { max_rows: Some(0) })
                .expect_err("incomplete rows must fail before row_count is reported");
        assert!(
            err.to_string().contains("incomplete row")
                && err.to_string().contains("1 cells for 2 columns"),
            "got: {err}",
        );
    }

    #[test]
    fn decode_limited_query_over_max_rows_does_not_error_when_returning_sample() {
        let row_count = MAX_ROWS + 1;
        let cells = vec![CellType::CellVarint as i32; row_count];
        let varint_cells: Vec<i64> = (0..row_count as i64).collect();
        let batch = CellsBatch {
            cells,
            varint_cells,
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: None,
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["n"], vec![batch]);
        let decoded =
            decode_query_result_with_options(&result, DecodeQueryOptions { max_rows: Some(10) })
                .unwrap();

        assert_eq!(decoded.row_count, MAX_ROWS + 1);
        assert_eq!(decoded.table.rows.len(), 10);
        assert!(decoded.rows_truncated);
    }

    #[test]
    fn decode_multi_batch() {
        let batch1 = CellsBatch {
            cells: vec![
                CellType::CellVarint as i32,
                CellType::CellString as i32,
                CellType::CellVarint as i32,
                CellType::CellString as i32,
            ],
            varint_cells: vec![1, 2],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: Some("a\0b".to_owned()),
            is_last_batch: Some(false),
        };
        let batch2 = CellsBatch {
            cells: vec![CellType::CellVarint as i32, CellType::CellString as i32],
            varint_cells: vec![3],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: Some("c".to_owned()),
            is_last_batch: Some(true),
        };
        let result = make_result(vec!["id", "name"], vec![batch1, batch2]);
        let table = decode_query_result(&result).unwrap();

        assert_eq!(table.columns, vec!["id", "name"]);
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.rows[0], vec![Value::from(1), Value::from("a")]);
        assert_eq!(table.rows[1], vec![Value::from(2), Value::from("b")]);
        assert_eq!(table.rows[2], vec![Value::from(3), Value::from("c")]);
    }

    #[test]
    fn decoded_table_len_and_is_empty() {
        let empty = DecodedTable {
            columns: vec![],
            rows: vec![],
        };
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        let two = DecodedTable {
            columns: vec!["a".into()],
            rows: vec![vec![Value::from(1)], vec![Value::from(2)]],
        };
        assert!(!two.is_empty());
        assert_eq!(two.len(), 2);
    }

    #[test]
    fn cell_lookup_finds_value_by_column_name() {
        let table = DecodedTable {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![Value::from(1), Value::String("x".into())]],
        };
        assert_eq!(table.cell(0, "a"), Some(&Value::from(1)));
        assert_eq!(table.cell(0, "b"), Some(&Value::String("x".into())));
    }

    #[test]
    fn cell_lookup_returns_none_for_unknown_column() {
        let table = DecodedTable {
            columns: vec!["a".into()],
            rows: vec![vec![Value::from(1)]],
        };
        assert!(table.cell(0, "missing").is_none());
    }

    #[test]
    fn cell_lookup_returns_none_for_out_of_range_row() {
        let table = DecodedTable {
            columns: vec!["a".into()],
            rows: vec![vec![Value::from(1)]],
        };
        assert!(table.cell(99, "a").is_none());
    }

    #[test]
    fn serialize_emits_canonical_columnar_shape() {
        let table = DecodedTable {
            columns: vec!["a".into(), "b".into()],
            rows: vec![
                vec![Value::from(1), Value::String("x".into())],
                vec![Value::from(2), Value::String("y".into())],
            ],
        };
        let value = serde_json::to_value(&table).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "columns": ["a", "b"],
                "rows": [[1, "x"], [2, "y"]],
            }),
        );
    }

    #[test]
    fn serialize_on_empty_table_emits_empty_columns_and_rows() {
        let table = DecodedTable {
            columns: vec![],
            rows: vec![],
        };
        let value = serde_json::to_value(&table).expect("serialize");
        assert_eq!(value, serde_json::json!({"columns": [], "rows": []}));
    }

    /// `decode_query_result` must preserve `proto.column_names` order
    /// verbatim — no alphabetization. Locks in the SELECT-clause-order
    /// behavior introduced by this refactor.
    #[test]
    fn decode_preserves_proto_column_order() {
        let batch = CellsBatch {
            cells: vec![
                CellType::CellVarint as i32,
                CellType::CellVarint as i32,
                CellType::CellVarint as i32,
            ],
            varint_cells: vec![10, 20, 30],
            float64_cells: vec![],
            blob_cells: vec![],
            string_cells: None,
            is_last_batch: Some(true),
        };
        // Deliberately non-alphabetical column order.
        let result = make_result(vec!["c", "a", "b"], vec![batch]);
        let table = decode_query_result(&result).unwrap();
        assert_eq!(
            table.columns,
            vec!["c", "a", "b"],
            "decode_query_result must preserve proto.column_names verbatim",
        );
        assert_eq!(
            table.rows[0],
            vec![Value::from(10), Value::from(20), Value::from(30)]
        );
    }
}
