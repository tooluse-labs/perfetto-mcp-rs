// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use rmcp::schemars;
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

pub const REDACT_STRINGS_DEFAULT_ENV: &str = "PERFETTO_MCP_REDACT_STRINGS_DEFAULT";

// Note: serde aliases (`#[serde(alias = "trace_path")]`) are recognized as
// the field they alias, so they don't trigger the unknown-field error.
//
// Numeric fields use the `lenient_*` deserializers below to also accept
// JSON-string-of-number forms (`"12800"` as well as `12800`). Motivated by a
// v0.11.2 Claude Code session that consistently stringified every numeric
// argument and bounced 4 times before giving up. JsonSchema still advertises
// `integer`/`number` so well-behaved LLMs see strict types; the deserializer
// is only a safety net for the LLMs that don't.

/// Deserialize an `Option<i64>` that also accepts a JSON string holding a
/// signed integer. Returns `None` for `null` or missing.
pub(crate) fn lenient_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => n.as_i64().map(Some).ok_or_else(|| {
            D::Error::custom(format!("integer out of i64 range or non-integral: {n}"))
        }),
        serde_json::Value::String(s) => s.parse::<i64>().map(Some).map_err(|e| {
            D::Error::custom(format!(
                "expected integer or numeric string, got string {s:?}: {e}"
            ))
        }),
        other => Err(D::Error::custom(format!(
            "expected integer or numeric string, got {other}"
        ))),
    }
}

/// `Option<f64>` analogue of `lenient_i64`.
pub(crate) fn lenient_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => n
            .as_f64()
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("number not representable as f64: {n}"))),
        serde_json::Value::String(s) => s.parse::<f64>().map(Some).map_err(|e| {
            D::Error::custom(format!(
                "expected number or numeric string, got string {s:?}: {e}"
            ))
        }),
        other => Err(D::Error::custom(format!(
            "expected number or numeric string, got {other}"
        ))),
    }
}

/// `Option<u32>` analogue of `lenient_i64`. Rejects negative numbers and
/// values exceeding `u32::MAX`.
pub(crate) fn lenient_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => n
            .as_u64()
            .filter(|&v| v <= u32::MAX as u64)
            .map(|v| Some(v as u32))
            .ok_or_else(|| D::Error::custom(format!("expected u32 (0..={}), got {n}", u32::MAX))),
        serde_json::Value::String(s) => s.parse::<u32>().map(Some).map_err(|e| {
            D::Error::custom(format!(
                "expected unsigned integer or numeric string, got string {s:?}: {e}"
            ))
        }),
        other => Err(D::Error::custom(format!(
            "expected unsigned integer or numeric string, got {other}"
        ))),
    }
}

/// Deserialize a vector of i64 values while accepting common LLM variants:
/// a singleton number/string, or an array mixing numbers and numeric strings.
pub(crate) fn lenient_i64_vec<'de, D>(deserializer: D) -> Result<Vec<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    fn parse_value<E>(value: serde_json::Value) -> Result<i64, E>
    where
        E: serde::de::Error,
    {
        match value {
            serde_json::Value::Number(n) => n
                .as_i64()
                .ok_or_else(|| E::custom(format!("integer out of i64 range or non-integral: {n}"))),
            serde_json::Value::String(s) => s.parse::<i64>().map_err(|e| {
                E::custom(format!(
                    "expected integer or numeric string, got string {s:?}: {e}"
                ))
            }),
            other => Err(E::custom(format!(
                "expected integer or numeric string, got {other}"
            ))),
        }
    }

    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(parse_value::<D::Error>)
            .collect::<Result<Vec<_>, _>>(),
        other => parse_value::<D::Error>(other).map(|value| vec![value]),
    }
}

pub(crate) fn default_redact_strings() -> bool {
    let value = std::env::var(REDACT_STRINGS_DEFAULT_ENV).ok();
    redact_strings_default_from_env_value(value.as_deref())
}

pub(crate) fn redact_strings_default_from_env_value(value: Option<&str>) -> bool {
    match value.map(|v| v.trim().to_ascii_lowercase()) {
        None => true,
        Some(value) if matches!(value.as_str(), "" | "1" | "true" | "yes" | "y" | "on") => true,
        Some(value) if matches!(value.as_str(), "0" | "false" | "no" | "n" | "off") => false,
        Some(_) => true,
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadTraceParams {
    /// Absolute path to a Perfetto trace file (.pftrace, .perfetto-trace, .bin,
    /// or any other trace_processor-readable format — content-sniffed, not by extension).
    #[serde(alias = "trace_path")]
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteSqlParams {
    /// SQL query to execute (PerfettoSQL syntax).
    pub sql: String,
    /// Optional output row cap. This trims returned rows only; it does not
    /// rewrite or limit the SQL that trace_processor executes. Mutually
    /// exclusive with `head`. Accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub limit: Option<u32>,
    /// Agent-friendly alias for `limit`: return only the first N decoded rows.
    /// This trims returned rows only; it does not rewrite the SQL. Mutually
    /// exclusive with `limit`. Accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub head: Option<u32>,
    /// Return only column names and row-count metadata; omit row values.
    #[serde(default)]
    pub columns_only: bool,
    /// Return column names, row-count metadata, and a small sample of rows.
    /// Defaults to 10 sample rows unless `head` or `limit` is provided.
    #[serde(default)]
    pub summary: bool,
    /// Include decoded row-count metadata with a row-returning shaped response.
    #[serde(default)]
    pub include_row_count: bool,
    /// Optional per-string-cell character cap applied to returned rows only.
    /// Accepts both numbers and numeric strings. Must be > 0 when set.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub max_string_len: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTablesParams {
    /// Optional GLOB pattern to filter table names (e.g. "chrome_*").
    #[serde(default)]
    pub pattern: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableStructureParams {
    /// Name of the table to describe. Also accepted as `name` for callers
    /// who model schema discovery around a generic "name" field.
    #[serde(alias = "name")]
    pub table_name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListProcessesParams {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListThreadsInProcessParams {
    /// Process upid (the trace-internal unique id from `list_processes`).
    /// Takes precedence over `process_name` when both are set — useful for
    /// disambiguating same-named processes (e.g. multiple Renderer instances).
    /// Accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_i64")]
    pub upid: Option<i64>,
    /// Process name to match exactly (e.g. "com.android.chrome",
    /// "/system/bin/init"). Either this or `upid` must be provided.
    #[serde(default)]
    pub process_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SliceDescendantsBreakdownParams {
    /// Root slice ids to expand. The root slices themselves are omitted from
    /// the summary; returned rows aggregate matching descendants under each
    /// root. Accepts numbers or numeric strings.
    #[serde(deserialize_with = "lenient_i64_vec")]
    pub slice_ids: Vec<i64>,
    /// Optional descendant minimum duration in milliseconds. Defaults to 1 ms.
    /// Must be finite and non-negative; accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_f64")]
    pub min_dur_ms: Option<f64>,
    /// Optional maximum descendant depth. Defaults to 8. Must be > 0 when set;
    /// accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub max_depth: Option<u32>,
    /// Include an example args summary for one representative slice per group.
    #[serde(default)]
    pub include_args: bool,
    /// Optional max rows to return. Defaults to 100 and is capped at 5000.
    /// Accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub limit: Option<u32>,
    /// Optional per-string-cell character cap applied to returned rows only.
    /// Unset preserves full strings for precision; accepts both numbers and
    /// numeric strings. Must be > 0 when set.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub max_string_len: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChromeTraceParams {
    /// Optional per-string-cell character cap applied to returned Chrome-tool
    /// rows only. Unset preserves full strings for precision; accepts both
    /// numbers and numeric strings. Must be > 0 when set.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub max_string_len: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChromeMainThreadHotspotsPhase {
    /// Window from navigation_start_ts to fcp_ts.
    NavigationToFcp,
    /// Window from navigation_start_ts to load_event_ts.
    NavigationToLoad,
    /// Window from dom_content_loaded_event_ts to fcp_ts.
    DclToFcp,
    /// Window from fcp_ts to load_event_ts.
    FcpToLoad,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChromeMainThreadHotspotsParams {
    /// Optional process-name filter (e.g. "Renderer", "Browser", "GPU Process").
    /// Useful to scope to one process type without picking a specific instance.
    #[serde(default)]
    pub process_name: Option<String>,
    /// Optional pid filter — the OS-level process ID (visible in Task Manager).
    /// Get pid from `list_processes`. ANDs with the other filters when set.
    /// Note: pids can be recycled within a long trace; prefer `upid` when
    /// precision matters. Accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_i64")]
    pub pid: Option<i64>,
    /// Optional upid filter — the trace-internal Unique Process ID assigned by
    /// trace_processor (also from `list_processes`). Always uniquely identifies
    /// one process within a trace, even if the OS recycled its pid. Use this
    /// to disambiguate same-named or pid-recycled processes; ANDs with the
    /// other filters when set. Accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_i64")]
    pub upid: Option<i64>,
    /// Optional page-load id used to scope tasks to one navigation phase.
    /// Matches `chrome_page_loads.id`. Mutually exclusive with `navigation_id`.
    /// If set without `phase`, defaults to `navigation_to_fcp`.
    #[serde(default, deserialize_with = "lenient_i64")]
    pub page_load_id: Option<i64>,
    /// Optional Chrome navigation id used to scope tasks to one navigation phase.
    /// Matches `chrome_page_loads.navigation_id`. Mutually exclusive with
    /// `page_load_id`. If set without `phase`, defaults to `navigation_to_fcp`.
    #[serde(default, deserialize_with = "lenient_i64")]
    pub navigation_id: Option<i64>,
    /// Optional page-load phase window. If set without `page_load_id` or
    /// `navigation_id`, uses the latest page load in the trace. Values:
    /// navigation_to_fcp, navigation_to_load, dcl_to_fcp, fcp_to_load.
    #[serde(default)]
    pub phase: Option<ChromeMainThreadHotspotsPhase>,
    /// Optional raw trace timestamp lower bound in nanoseconds. This uses the
    /// same unit as the returned `ts` column. ANDs with any page-load window.
    #[serde(default, alias = "start_ts", deserialize_with = "lenient_i64")]
    pub start_ts_ns: Option<i64>,
    /// Optional raw trace timestamp upper bound in nanoseconds, exclusive. This
    /// uses the same unit as the returned `ts` column. ANDs with any page-load
    /// window.
    #[serde(default, alias = "end_ts", deserialize_with = "lenient_i64")]
    pub end_ts_ns: Option<i64>,
    /// Optional minimum task duration in milliseconds. Defaults to 16 ms (one
    /// 60 Hz frame budget). Pass 0 to see ALL main-thread tasks; raise to e.g.
    /// 33 (30 Hz) or 100 to focus on the worst stutters. Must be a finite
    /// non-negative number. Accepts both numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_f64")]
    pub min_dur_ms: Option<f64>,
    /// Optional max rows to return. Defaults to 100 and is capped at 5000 to
    /// match `execute_sql`. Lower values keep responses short; higher values
    /// surface long tails of mid-duration tasks. Accepts both numbers and
    /// numeric strings.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub limit: Option<u32>,
    /// Optional per-string-cell character cap applied to returned rows only.
    /// Unset preserves full strings for precision; accepts both numbers and
    /// numeric strings. Must be > 0 when set.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub max_string_len: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListStdlibModulesParams {
    /// Optional domain filter. Valid values: "chrome", "android", "generic".
    #[serde(default)]
    pub domain: Option<String>,
    /// Optional case-insensitive search over module name, view names, and description.
    #[serde(default)]
    pub query: Option<String>,
    /// Optional max entries to return. Must be > 0 when set; accepts both
    /// numbers and numeric strings.
    #[serde(default, deserialize_with = "lenient_u32")]
    pub limit: Option<u32>,
}

/// Output of `list_tables`. Just the matching names; the count is implicit
/// (`names.len()`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TableList {
    pub names: Vec<String>,
}

/// Output of `list_table_structure`. Mirrors the analyst-relevant subset of
/// SQLite's `PRAGMA table_info`. `cid`, `dflt_value`, and `pk` are omitted
/// because nothing in the analysis path needs them today; trivial to add
/// later if a caller surfaces a need.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TableInfo {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ColumnInfo {
    pub name: String,
    /// SQLite type name (`INTEGER`, `TEXT`, `REAL`, ...). `#[serde(rename)]`
    /// because `type` is a reserved word on the Rust side.
    #[serde(rename = "type")]
    pub data_type: String,
    /// Inverse of SQLite's `notnull` flag: `nullable = (notnull == 0)`.
    pub nullable: bool,
}

/// Tunable filters for `chrome_main_thread_hotspots_sql`. All fields are
/// `Option`-of-something so callers can spread `..Default::default()` and
/// only set the knobs they care about — much more readable than 5 positional
/// `Option<T>` arguments at the call site.
///
/// `Copy` so the builder fn can take it by value without `.clone()` ceremony;
/// the string borrow makes the whole struct lifetime-parameterized but in
/// practice every call site has a `'static` literal or a borrow that
/// outlives the SQL build. Exported for integration tests.
#[derive(Default, Debug, Clone, Copy)]
pub struct ChromeMainThreadHotspotsFilters<'a> {
    /// Optional process-name filter (e.g. "Renderer", "Browser").
    pub process_name: Option<&'a str>,
    /// Optional OS pid filter — see `ChromeMainThreadHotspotsParams::pid`.
    pub pid: Option<i64>,
    /// Optional trace-internal upid filter — precise even when pid recycles.
    pub upid: Option<i64>,
    /// Optional page-load id/window scoping. Matches either `chrome_page_loads.id`
    /// or, when `navigation_id` is set, `chrome_page_loads.navigation_id`.
    pub page_load_id: Option<i64>,
    /// Optional Chrome navigation id/window scoping.
    pub navigation_id: Option<i64>,
    /// Optional page-load phase; defaults to navigation_to_fcp when a page-load
    /// or navigation id is set.
    pub phase: Option<ChromeMainThreadHotspotsPhase>,
    /// Optional raw trace timestamp lower bound in ns.
    pub start_ts_ns: Option<i64>,
    /// Optional raw trace timestamp upper bound in ns, exclusive.
    pub end_ts_ns: Option<i64>,
    /// Optional override of the default 16 ms threshold (ms; must be
    /// finite non-negative, finite when multiplied to ns).
    pub min_dur_ms: Option<f64>,
    /// Optional override of the default `LIMIT 100`. Capped at `MAX_ROWS`.
    /// Must be `> 0` if set.
    pub limit: Option<u32>,
}

/// Tunable filters for `slice_descendants_breakdown_sql`. `slice_ids` is the
/// raw caller input (validated and deduped inside the SQL builder).
/// `row_limit` is the *already-resolved* effective limit — the handler runs
/// `slice_descendants_effective_limit` once before building the SQL so the
/// limit cannot drift between the LIMIT clause and the response envelope.
#[derive(Debug, Clone, Copy)]
pub struct SliceDescendantsBreakdownFilters<'a> {
    pub slice_ids: &'a [i64],
    pub min_dur_ms: Option<f64>,
    pub max_depth: Option<u32>,
    pub include_args: bool,
    pub row_limit: u32,
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct I64Wrapper {
        #[serde(default, deserialize_with = "lenient_i64")]
        val: Option<i64>,
    }

    #[derive(Deserialize)]
    struct F64Wrapper {
        #[serde(default, deserialize_with = "lenient_f64")]
        val: Option<f64>,
    }

    #[derive(Deserialize)]
    struct U32Wrapper {
        #[serde(default, deserialize_with = "lenient_u32")]
        val: Option<u32>,
    }

    #[test]
    fn lenient_i64_accepts_number_string_and_null() {
        let from_num: I64Wrapper = serde_json::from_str(r#"{"val": 42}"#).unwrap();
        assert_eq!(from_num.val, Some(42));

        let from_str: I64Wrapper = serde_json::from_str(r#"{"val": "12800"}"#).unwrap();
        assert_eq!(from_str.val, Some(12800));

        let from_neg_str: I64Wrapper = serde_json::from_str(r#"{"val": "-7"}"#).unwrap();
        assert_eq!(from_neg_str.val, Some(-7));

        let from_null: I64Wrapper = serde_json::from_str(r#"{"val": null}"#).unwrap();
        assert_eq!(from_null.val, None);

        let from_missing: I64Wrapper = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(from_missing.val, None);
    }

    #[test]
    fn lenient_i64_rejects_garbage_string() {
        let result = serde_json::from_str::<I64Wrapper>(r#"{"val": "abc"}"#);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("expected integer or numeric string"),
            "error should explain the constraint: {err_msg}",
        );
    }

    #[test]
    fn lenient_f64_accepts_number_and_string() {
        let from_num: F64Wrapper = serde_json::from_str(r#"{"val": 1.25}"#).unwrap();
        assert!((from_num.val.unwrap() - 1.25).abs() < f64::EPSILON);

        let from_str: F64Wrapper = serde_json::from_str(r#"{"val": "2.5"}"#).unwrap();
        assert!((from_str.val.unwrap() - 2.5).abs() < f64::EPSILON);

        let from_int: F64Wrapper = serde_json::from_str(r#"{"val": 100}"#).unwrap();
        assert!((from_int.val.unwrap() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn lenient_u32_accepts_and_rejects_correctly() {
        let ok: U32Wrapper = serde_json::from_str(r#"{"val": 500}"#).unwrap();
        assert_eq!(ok.val, Some(500));

        let from_str: U32Wrapper = serde_json::from_str(r#"{"val": "123"}"#).unwrap();
        assert_eq!(from_str.val, Some(123));

        let neg = serde_json::from_str::<U32Wrapper>(r#"{"val": -1}"#);
        assert!(neg.is_err());

        let too_big = serde_json::from_str::<U32Wrapper>(r#"{"val": 5000000000}"#);
        assert!(too_big.is_err());
    }

    #[test]
    fn lenient_i64_vec_accepts_arrays_and_singletons() {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "lenient_i64_vec")]
            val: Vec<i64>,
        }

        let from_array: Wrapper = serde_json::from_str(r#"{"val": [1, "2"]}"#).unwrap();
        assert_eq!(from_array.val, vec![1, 2]);

        let from_single: Wrapper = serde_json::from_str(r#"{"val": "3"}"#).unwrap();
        assert_eq!(from_single.val, vec![3]);

        let invalid = serde_json::from_str::<Wrapper>(r#"{"val": ["nope"]}"#);
        assert!(invalid.is_err());
    }

    #[test]
    fn redact_strings_default_is_privacy_first_and_env_can_disable() {
        assert!(redact_strings_default_from_env_value(None));
        assert!(redact_strings_default_from_env_value(Some("")));
        assert!(redact_strings_default_from_env_value(Some("true")));
        assert!(redact_strings_default_from_env_value(Some("1")));
        assert!(redact_strings_default_from_env_value(Some("unexpected")));
        assert!(!redact_strings_default_from_env_value(Some("false")));
        assert!(!redact_strings_default_from_env_value(Some("0")));
        assert!(!redact_strings_default_from_env_value(Some("off")));
        assert!(!redact_strings_default_from_env_value(Some("no")));
    }

    #[test]
    fn deny_unknown_fields_rejects_extra_keys() {
        let result = serde_json::from_str::<ListStdlibModulesParams>(r#"{"bogus": 1}"#);
        assert!(
            result.is_err(),
            "deny_unknown_fields must reject extra keys"
        );
    }

    #[test]
    fn list_stdlib_modules_params_accept_filters() {
        let params: ListStdlibModulesParams =
            serde_json::from_str(r#"{"domain": "chrome", "query": "jank", "limit": "2"}"#)
                .expect("filters must deserialize");
        assert_eq!(params.domain.as_deref(), Some("chrome"));
        assert_eq!(params.query.as_deref(), Some("jank"));
        assert_eq!(params.limit, Some(2));
    }

    #[test]
    fn slice_descendants_breakdown_params_accept_llm_numeric_variants() {
        let params: SliceDescendantsBreakdownParams = serde_json::from_str(
            r#"{"slice_ids": ["42", 43], "min_dur_ms": "0.5", "max_depth": "4", "limit": "20", "max_string_len": "240"}"#,
        )
        .expect("stringified numeric fields must deserialize");

        assert_eq!(params.slice_ids, vec![42, 43]);
        assert_eq!(params.min_dur_ms, Some(0.5));
        assert_eq!(params.max_depth, Some(4));
        assert_eq!(params.limit, Some(20));
        assert_eq!(params.max_string_len, Some(240));
    }

    #[test]
    fn slice_descendants_breakdown_params_require_slice_ids() {
        let err = serde_json::from_str::<SliceDescendantsBreakdownParams>(r#"{}"#)
            .expect_err("slice_ids must be required in the advertised schema");
        assert!(
            err.to_string().contains("slice_ids"),
            "error must name the missing field, got: {err}",
        );
    }
}
