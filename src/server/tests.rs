use super::*;
use serde_json::json;
use std::path::PathBuf;

fn compact_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

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

fn decoded_table(columns: &[&str], rows: Vec<Vec<serde_json::Value>>) -> DecodedTable {
    DecodedTable {
        columns: columns.iter().map(|column| (*column).to_owned()).collect(),
        rows,
    }
}

fn execute_sql_params(sql: &str) -> ExecuteSqlParams {
    ExecuteSqlParams {
        sql: sql.to_owned(),
        limit: None,
        head: None,
        columns_only: false,
        summary: false,
        include_row_count: false,
        max_string_len: None,
    }
}

fn assert_json_key_order(response: &str, first: &str, second: &str) {
    let first_pos = response
        .find(first)
        .unwrap_or_else(|| panic!("missing key fragment {first:?} in {response}"));
    let second_pos = response
        .find(second)
        .unwrap_or_else(|| panic!("missing key fragment {second:?} in {response}"));
    assert!(
        first_pos < second_pos,
        "expected {first:?} before {second:?}, got: {response}"
    );
}

#[test]
fn load_trace_summary_classifies_android_chrome_trace() {
    let metadata = decoded_table(
        &["name", "str_value", "int_value"],
        vec![
            vec![json!("trace_type"), json!("proto"), serde_json::Value::Null],
            vec![
                json!("android_build_fingerprint"),
                json!("google/oriole/oriole:13/TP1A.220624.014/8819323:userdebug/dev-keys"),
                serde_json::Value::Null,
            ],
            vec![
                json!("android_sdk_version"),
                serde_json::Value::Null,
                json!(33),
            ],
            vec![
                json!("cr-product-version"),
                json!("Chrome/121.0.6167.178"),
                serde_json::Value::Null,
            ],
        ],
    );
    let overview = decoded_table(
        &[
            "start_ts",
            "end_ts",
            "duration_ns",
            "process_count",
            "thread_count",
            "has_slices",
            "has_counters",
            "has_sched",
            "has_ftrace",
            "has_chrome",
        ],
        vec![vec![
            json!(1_000),
            json!(2_001_000),
            json!(2_000_000),
            json!(5),
            json!(14),
            json!(1),
            json!(1),
            json!(1),
            json!(1),
            json!(1),
        ]],
    );

    let summary = build_load_trace_summary(&metadata, &overview, Some(6_392_331));

    assert!(summary.available);
    assert_eq!(summary.trace_type.as_deref(), Some("proto"));
    assert_eq!(summary.trace_profile, "chrome");
    assert_eq!(summary.platform.as_deref(), Some("Android"));
    assert_eq!(summary.android_sdk_version, Some(33));
    assert_eq!(
        summary.chrome_product_version.as_deref(),
        Some("Chrome/121.0.6167.178")
    );
    assert_eq!(summary.duration_ms, Some(2.0));
    assert_eq!(summary.process_count, Some(5));
    assert_eq!(summary.thread_count, Some(14));
    assert_eq!(
        summary.capabilities,
        vec!["chrome", "android", "sched", "ftrace", "slices", "counters"]
    );
    assert!(
        summary
            .recommended_next_tools
            .contains(&"chrome_main_thread_hotspots".to_owned()),
        "Chrome summary must route callers to dedicated chrome tools: {summary:?}",
    );
    assert!(summary.warnings.is_empty());
}

#[test]
fn load_trace_response_embeds_parseable_summary_json() {
    let summary = LoadTraceSummary {
        available: true,
        trace_type: Some("proto".to_owned()),
        trace_profile: "generic".to_owned(),
        platform: Some("Linux (x86_64)".to_owned()),
        android_build_fingerprint: None,
        android_sdk_version: None,
        chrome_product_version: None,
        start_ts: Some(10),
        end_ts: Some(20),
        duration_ms: Some(0.00001),
        file_size_bytes: Some(207),
        process_count: Some(4),
        thread_count: Some(4),
        capabilities: vec!["slices".to_owned()],
        recommended_next_tools: recommended_tools("generic"),
        redaction_policy: redaction_policy_for(true),
        warnings: vec![],
    };

    let response = format_load_trace_response("/tmp/basic.perfetto-trace", Ok(summary))
        .expect("response must serialize");
    let summary_line = response
        .lines()
        .find_map(|line| line.strip_prefix("Trace summary: "))
        .expect("response must contain a Trace summary line");
    let parsed: serde_json::Value =
        serde_json::from_str(summary_line).expect("summary line must be JSON");

    assert_eq!(parsed["available"], json!(true));
    assert_eq!(parsed["trace_profile"], json!("generic"));
    assert_eq!(parsed["process_count"], json!(4));
    assert!(
        response.contains("recommended_next_tools"),
        "response should expose routing data, got: {response}",
    );
    assert_eq!(
        parsed["redaction_policy"]["execute_sql_string_cells"],
        json!(true),
        "response should expose current privacy policy, got: {response}",
    );
    assert_eq!(
        parsed["redaction_policy"]["chrome_tool_string_cells"],
        json!(true),
        "response should expose Chrome tool privacy policy, got: {response}",
    );
    assert_eq!(
        parsed["redaction_policy"]["env_var"],
        json!(REDACT_STRINGS_DEFAULT_ENV)
    );
}

#[test]
fn load_trace_response_preserves_success_when_summary_unavailable() {
    let response = format_load_trace_response("/tmp/basic.perfetto-trace", Err("boom".into()))
        .expect("response must serialize");
    let summary_line = response
        .lines()
        .find_map(|line| line.strip_prefix("Trace summary: "))
        .expect("response must contain a Trace summary line");
    let parsed: serde_json::Value =
        serde_json::from_str(summary_line).expect("summary line must be JSON");

    assert_eq!(parsed["available"], json!(false));
    assert_eq!(parsed["error"], json!("boom"));
    assert!(
        parsed.get("redaction_policy").is_some(),
        "summary failure must still expose privacy policy: {response}",
    );
    assert!(
        response.starts_with("Trace loaded successfully"),
        "summary failure must not make load_trace look failed: {response}",
    );
}

#[test]
fn schema_cache_is_scoped_to_trace_fingerprint() {
    let key_a = SchemaCacheTraceKey {
        canonical_path: PathBuf::from("/tmp/a.perfetto-trace"),
        size_bytes: 1,
        modified: None,
        platform: Some("same-platform".to_owned()),
        sample_sha256: "old".to_owned(),
    };
    let key_b = SchemaCacheTraceKey {
        canonical_path: PathBuf::from("/tmp/a.perfetto-trace"),
        size_bytes: 1,
        modified: None,
        platform: Some("same-platform".to_owned()),
        sample_sha256: "new".to_owned(),
    };
    let mut cache = SchemaCache::default();

    cache.store_table_list(key_a.clone(), None, r#"{"names":["slice"]}"#.to_owned());
    cache.store_table_structure(
        key_a.clone(),
        "slice".to_owned(),
        r#"{"table":"slice","columns":[]}"#.to_owned(),
    );

    assert_eq!(
        cache.table_list(&key_a, &None).as_deref(),
        Some(r#"{"names":["slice"]}"#)
    );
    assert!(cache.table_list(&key_b, &None).is_none());
    assert!(
        cache.table_structure(&key_a, "slice").is_none(),
        "switching trace fingerprints must clear stale structures too"
    );
}

#[test]
fn load_trace_returns_summary_from_real_trace() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = Arc::new(TraceProcessorManager::new_with_starting_port(1, 19_061));
        let server = PerfettoMcpServer::new(manager);
        let response = server
            .load_trace(Parameters(LoadTraceParams {
                path: "tests/fixtures/basic.perfetto-trace".to_owned(),
            }))
            .await
            .expect("load_trace on basic fixture must succeed");

        let summary_line = response
            .lines()
            .find_map(|line| line.strip_prefix("Trace summary: "))
            .expect("load_trace must include a summary line");
        let parsed: serde_json::Value =
            serde_json::from_str(summary_line).expect("summary line must be JSON");

        assert_eq!(parsed["available"], json!(true));
        assert_eq!(parsed["trace_type"], json!("proto"));
        assert_eq!(parsed["trace_profile"], json!("generic"));
        assert_eq!(parsed["process_count"], json!(4));
        assert_eq!(parsed["thread_count"], json!(4));
        assert!(
            parsed["duration_ms"].as_f64().is_some(),
            "trace_dur() duration should be present: {parsed}",
        );
        assert!(
            parsed["recommended_next_tools"]
                .as_array()
                .expect("recommended_next_tools must be an array")
                .iter()
                .any(|tool| tool.as_str() == Some("execute_sql")),
            "summary should preserve execute_sql as an escape hatch: {parsed}",
        );
    });
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
fn execute_sql_hint_fires_on_missing_module() {
    let formatted = format_execute_sql_error(PerfettoError::QueryError {
        kind: QueryErrorKind::MissingModule,
        message: "INCLUDE: unknown module 'chrome.page_load'".to_owned(),
    });
    assert!(
        formatted.contains("list_stdlib_modules"),
        "missing-module errors must point at stdlib discovery, got: {formatted}",
    );
    assert!(
        formatted.contains("PERFETTO_TP_PATH"),
        "missing-module hint must mention binary-version drift, got: {formatted}",
    );
}

#[test]
fn execute_sql_hint_fires_on_multiple_output_statements() {
    let formatted = format_execute_sql_error(PerfettoError::QueryError {
        kind: QueryErrorKind::MultipleOutputStatements,
        message: "SQL returned rows from 2 output statements".to_owned(),
    });
    assert!(
        formatted.contains("at most one statement produces rows"),
        "multi-output hint must explain the one-output contract, got: {formatted}",
    );
    assert!(
        formatted.contains("INCLUDE PERFETTO MODULE"),
        "multi-output hint must preserve INCLUDE+SELECT as valid, got: {formatted}",
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

#[test]
fn execute_sql_response_without_shaping_preserves_legacy_shape() {
    let table = decoded_table(&["a"], vec![vec![json!("abcdef")]]);
    let params = execute_sql_params("SELECT 'abcdef' AS a");

    let response =
        format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(
        parsed,
        json!({"columns": ["a"], "rows": [["abcdef"]]}),
        "default execute_sql response must stay wire-compatible",
    );
}

#[test]
fn chrome_tool_response_adds_metadata_before_rows_without_dropping_rows() {
    let table = decoded_table(
        &["id", "url"],
        vec![
            vec![json!(1), json!("https://example.test/a")],
            vec![json!(2), json!("https://example.test/b")],
        ],
    );

    let response =
        format_chrome_tool_response_with_redaction(table, 2, DEFAULT_TOOL_MAX_STRING_LEN, false)
            .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_json_key_order(&response, "\"truncated\":", "\"rows\":");
    assert_eq!(parsed["columns"], json!(["id", "url"]));
    assert_eq!(
        parsed["rows"],
        json!([[1, "https://example.test/a"], [2, "https://example.test/b"]])
    );
    assert_eq!(parsed["row_count"], serde_json::Value::Null);
    assert_eq!(parsed["returned_rows"], json!(2));
    assert_eq!(parsed["truncated"], json!(true));
    assert_eq!(parsed["row_count_known"], json!(false));
    assert_eq!(parsed["redacted"], json!(false));
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("row_count unknown"),
        "note must explain Chrome tool completeness metadata: {parsed}",
    );
}

#[test]
fn chrome_tool_response_with_known_row_count_only_truncates_when_more_rows_exist() {
    let table = decoded_table(&["id"], vec![vec![json!(1)], vec![json!(2)]]);

    let response = format_chrome_tool_response_with_known_row_count_and_redaction(
        table,
        2,
        DEFAULT_TOOL_MAX_STRING_LEN,
        false,
    )
    .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_json_key_order(&response, "\"truncated\":", "\"rows\":");
    assert_eq!(parsed["row_count"], json!(2));
    assert_eq!(parsed["returned_rows"], json!(2));
    assert_eq!(parsed["truncated"], json!(false));
    assert_eq!(parsed["row_count_known"], json!(true));
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("more rows exist than returned"),
        "note must explain exact truncation semantics: {parsed}",
    );

    let truncated_table = decoded_table(&["id"], vec![vec![json!(1)], vec![json!(2)]]);
    let truncated_response = format_chrome_tool_response_with_known_row_count_and_redaction(
        truncated_table,
        3,
        DEFAULT_TOOL_MAX_STRING_LEN,
        false,
    )
    .expect("serialize");
    let truncated: serde_json::Value = serde_json::from_str(&truncated_response).expect("json");
    assert_eq!(truncated["row_count"], json!(3));
    assert_eq!(truncated["returned_rows"], json!(2));
    assert_eq!(truncated["truncated"], json!(true));
}

#[test]
fn list_threads_response_exposes_exact_count_and_truncation() {
    let table = decoded_table(
        &["tid", "thread_name", "pid", "upid"],
        vec![vec![
            json!(10),
            json!("CrRendererMain"),
            json!(100),
            json!(1),
        ]],
    );

    let response = format_list_threads_response(table, 2).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_json_key_order(&response, "\"truncated\":", "\"rows\":");
    assert_eq!(parsed["row_count"], json!(2));
    assert_eq!(parsed["returned_rows"], json!(1));
    assert_eq!(parsed["truncated"], json!(true));
    assert_eq!(parsed["row_count_known"], json!(true));
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("2000-row cap"),
        "note must explain list_threads truncation: {parsed}",
    );
}

#[test]
fn list_threads_response_marks_complete_result_not_truncated() {
    let table = decoded_table(
        &["tid", "thread_name", "pid", "upid"],
        vec![vec![
            json!(10),
            json!("CrRendererMain"),
            json!(100),
            json!(1),
        ]],
    );

    let response = format_list_threads_response(table, 1).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(parsed["row_count"], json!(1));
    assert_eq!(parsed["returned_rows"], json!(1));
    assert_eq!(parsed["truncated"], json!(false));
}

#[test]
fn decoded_row_count_rejects_missing_or_invalid_count_cell() {
    let missing = decoded_table(&["n"], vec![vec![json!(1)]]);
    let err = decoded_row_count(&missing, "test_count").expect_err("missing count must reject");
    assert!(err.contains("row_count"), "got: {err}");

    let invalid = decoded_table(&["row_count"], vec![vec![json!("not-a-number")]]);
    let err = decoded_row_count(&invalid, "test_count").expect_err("invalid count must reject");
    assert!(err.contains("row_count"), "got: {err}");
}

#[test]
fn chrome_tool_response_uses_server_side_string_redaction() {
    let table = decoded_table(
        &["url"],
        vec![vec![json!(
            "https://px.effirst.com/api/v1/otelconfig?wpk-header=secret&ok=1"
        )]],
    );

    let response = format_chrome_tool_response_with_redaction(
        table,
        DEFAULT_CHROME_TOOL_ROWS,
        DEFAULT_TOOL_MAX_STRING_LEN,
        true,
    )
    .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    let redacted_url = parsed["rows"][0][0].as_str().expect("redacted URL");
    assert!(
        redacted_url.starts_with("https://px.effirst.com/api/v1/otelconfig?wpk-header=<redacted:")
            && redacted_url.ends_with(">&ok=1"),
        "sensitive URL value must be redacted with a stable placeholder: {redacted_url}",
    );
    assert_eq!(parsed["redacted"], json!(true));
    assert_eq!(parsed["string_truncated"], json!(false));
}

#[test]
fn resource_summary_response_carries_attribution_evidence_before_rows() {
    let table = decoded_table(
        &["url_key", "max_overlap_ms"],
        vec![vec![json!("https://example.test/app.js"), json!(123.0)]],
    );
    let evidence = ChromeResourceTimingEvidence {
        attribution_scope: "url_lifecycle_span",
        phase_breakdown: "absent",
        phase_breakdown_available: false,
        safe_conclusion: "safe",
        safe_fact_fields: vec!["url lifecycle/request span"],
        unsafe_inferences: vec!["dns", "ttfb"],
        hypothesis_only: vec!["cdn/server latency"],
        network_phase_slice_count: 0,
        network_phase_arg_count: 0,
        incomplete_resource_slice_count: 1,
        incomplete_slices_excluded: true,
    };

    let response = format_chrome_resource_summary_response_with_redaction(
        table,
        DEFAULT_CHROME_TOOL_ROWS,
        DEFAULT_TOOL_MAX_STRING_LEN,
        false,
        evidence,
    )
    .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_json_key_order(&response, "\"resource_timing_evidence\":", "\"rows\":");
    assert_eq!(
        parsed["resource_timing_evidence"]["attribution_scope"],
        json!("url_lifecycle_span")
    );
    assert_eq!(
        parsed["resource_timing_evidence"]["phase_breakdown_available"],
        json!(false)
    );
    assert_eq!(
        parsed["resource_timing_evidence"]["unsafe_inferences"],
        json!(["dns", "ttfb"])
    );
    assert_eq!(
        parsed["resource_timing_evidence"]["incomplete_resource_slice_count"],
        json!(1)
    );
    assert_eq!(
        parsed["rows"],
        json!([["https://example.test/app.js", 123.0]])
    );
}

#[test]
fn resource_timing_evidence_probe_distinguishes_absent_and_present_phase_hints() {
    let absent = decoded_table(
        &[
            "network_phase_slice_count",
            "network_phase_arg_count",
            "incomplete_resource_slice_count",
            "phase_breakdown_available",
        ],
        vec![vec![json!(0), json!(0), json!(2), json!(0)]],
    );
    let absent_evidence = chrome_resource_timing_evidence_from_probe(&absent);
    assert_eq!(absent_evidence.attribution_scope, "url_lifecycle_span");
    assert_eq!(absent_evidence.phase_breakdown, "absent");
    assert!(absent_evidence.unsafe_inferences.contains(&"download"));
    assert_eq!(absent_evidence.incomplete_resource_slice_count, 2);

    let present = decoded_table(
        &[
            "network_phase_slice_count",
            "network_phase_arg_count",
            "incomplete_resource_slice_count",
            "phase_breakdown_available",
        ],
        vec![vec![json!(3), json!(1), json!(0), json!(1)]],
    );
    let present_evidence = chrome_resource_timing_evidence_from_probe(&present);
    assert_eq!(
        present_evidence.attribution_scope,
        "url_lifecycle_span_with_phase_hints"
    );
    assert_eq!(present_evidence.phase_breakdown, "phase_hints_present");
    assert!(present_evidence.phase_breakdown_available);
    assert!(present_evidence.unsafe_inferences.contains(&"ttfb"));
    assert_eq!(present_evidence.network_phase_slice_count, 3);
    assert_eq!(present_evidence.network_phase_arg_count, 1);
}

#[test]
fn chrome_tool_response_preserves_long_strings_by_default() {
    let long = "abcdefghijklmnopqrstuvwxyz".repeat(12);
    let table = decoded_table(&["name"], vec![vec![json!(long.clone())]]);

    let response =
        format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, None).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    let returned = parsed["rows"][0][0].as_str().expect("string cell");
    assert_eq!(
        returned, long,
        "Chrome-tool strings should be precise by default"
    );
    assert_eq!(parsed["string_truncated"], json!(false));
}

#[test]
fn chrome_tool_response_truncates_long_strings_when_requested() {
    let long = "abcdefghijklmnopqrstuvwxyz".repeat(12);
    let table = decoded_table(&["name"], vec![vec![json!(long)]]);

    let response =
        format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, Some(24)).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    let returned = parsed["rows"][0][0].as_str().expect("string cell");
    assert!(
        returned.ends_with("...<truncated>"),
        "explicit max_string_len should cap long Chrome-tool strings: {returned}"
    );
    assert_eq!(
        returned.chars().count(),
        24 + "...<truncated>".chars().count()
    );
    assert_eq!(parsed["string_truncated"], json!(true));
}

#[test]
fn chrome_tool_response_rejects_zero_max_string_len() {
    let table = decoded_table(&["name"], vec![vec![json!("short")]]);

    let err = format_chrome_tool_response(table, DEFAULT_CHROME_TOOL_ROWS, Some(0))
        .expect_err("zero max_string_len must reject");

    assert!(err.contains("max_string_len"), "got: {err}");
}

#[test]
fn slice_descendants_response_echoes_summary_bounds_before_rows() {
    let table = decoded_table(
        &[
            "root_id",
            "depth",
            "name",
            "slice_count",
            "inclusive_total_ms",
            "self_ms",
        ],
        vec![vec![
            json!(10),
            json!(1),
            json!("child"),
            json!(2),
            json!(3.5),
            json!(1.25),
        ]],
    );
    let applied_filters = SliceDescendantsAppliedFilters {
        min_dur_ms: DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
        max_depth: DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
        limit: DEFAULT_SLICE_DESCENDANTS_LIMIT,
        include_args: false,
    };

    let response = format_slice_descendants_tool_response_with_redaction(
        table,
        DEFAULT_SLICE_DESCENDANTS_LIMIT as usize,
        applied_filters,
        Vec::new(),
        None,
        false,
    )
    .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_json_key_order(&response, "\"applied_filters\":", "\"rows\":");
    assert_eq!(
        parsed["summary_scope"],
        json!(SLICE_DESCENDANTS_BREAKDOWN_SCOPE)
    );
    assert_eq!(
        parsed["applied_filters"],
        json!({
            "min_dur_ms": DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
            "max_depth": DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
            "limit": DEFAULT_SLICE_DESCENDANTS_LIMIT,
            "include_args": false,
        })
    );
    assert_eq!(
        parsed["missing_root_ids"],
        json!([] as [i64; 0]),
        "missing_root_ids must be present and empty when all roots existed: {parsed}",
    );
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("matching min_dur_ms within max_depth"),
        "note must explain bounded summary semantics: {parsed}",
    );
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("do not sum it across rows/depths"),
        "note must warn that inclusive totals overlap across depths: {parsed}",
    );
}

#[test]
fn slice_descendants_response_includes_missing_root_ids() {
    let table = decoded_table(&["root_id", "depth", "name", "slice_count"], vec![]);
    let applied_filters = SliceDescendantsAppliedFilters {
        min_dur_ms: DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
        max_depth: DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
        limit: DEFAULT_SLICE_DESCENDANTS_LIMIT,
        include_args: false,
    };

    let response = format_slice_descendants_tool_response_with_redaction(
        table,
        DEFAULT_SLICE_DESCENDANTS_LIMIT as usize,
        applied_filters,
        vec![42, 99],
        None,
        false,
    )
    .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(
        parsed["missing_root_ids"],
        json!([42, 99]),
        "missing_root_ids must echo back the stale ids in caller order: {parsed}",
    );
    assert_eq!(
        parsed["returned_rows"],
        json!(0),
        "empty descendants must still emit the envelope: {parsed}",
    );
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("missing_root_ids"),
        "shaping note must explain missing_root_ids semantics: {parsed}",
    );
}

#[test]
fn slice_descendants_applied_filters_use_effective_defaults() {
    let params = SliceDescendantsBreakdownParams {
        slice_ids: vec![10],
        min_dur_ms: None,
        max_depth: None,
        include_args: true,
        limit: None,
        max_string_len: None,
    };

    let filters = slice_descendants_applied_filters(&params, DEFAULT_SLICE_DESCENDANTS_LIMIT);

    assert_eq!(
        filters,
        SliceDescendantsAppliedFilters {
            min_dur_ms: DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS,
            max_depth: DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
            limit: DEFAULT_SLICE_DESCENDANTS_LIMIT,
            include_args: true,
        }
    );
}

#[test]
fn slice_descendants_error_hint_recommends_table_or_column_inspection() {
    let table_err = format_slice_descendants_tool_error(PerfettoError::QueryError {
        kind: QueryErrorKind::MissingTable,
        message: "no such table: slice".to_owned(),
    });
    assert!(
        table_err.contains("requires the base `slice` table") && table_err.contains("list_tables"),
        "missing-table hint must point at the schema check: {table_err}",
    );

    let column_err = format_slice_descendants_tool_error(PerfettoError::QueryError {
        kind: QueryErrorKind::MissingColumn,
        message: "no such column: slice.parent_id".to_owned(),
    });
    assert!(
        column_err.contains("list_table_structure") && column_err.contains("slice.id"),
        "missing-column hint must point at column inspection: {column_err}",
    );

    let other_err = format_slice_descendants_tool_error(PerfettoError::QueryError {
        kind: QueryErrorKind::Other,
        message: "syntax error near 'WITH'".to_owned(),
    });
    assert!(
        other_err.starts_with("Failed to run slice_descendants_breakdown:")
            && other_err.contains("syntax error"),
        "other errors must pass through with the tool prefix: {other_err}",
    );
}

#[test]
fn execute_sql_head_limits_returned_rows_and_marks_truncation() {
    let table = decoded_table(&["n"], vec![vec![json!(1)], vec![json!(2)]]);
    let mut params = execute_sql_params("SELECT n FROM nums");
    params.head = Some(1);

    let response =
        format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(parsed["rows"], json!([[1]]));
    assert_json_key_order(&response, "\"truncated\":", "\"rows\":");
    assert_eq!(parsed["row_count"], json!(2));
    assert_eq!(parsed["returned_rows"], json!(1));
    assert_eq!(parsed["truncated"], json!(true));
    assert_eq!(parsed["row_count_known"], json!(true));
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("post-SQL decoded rows"),
        "note must prevent SQL-limit confusion: {parsed}",
    );
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("capped at 5000"),
        "note must explain the output row cap: {parsed}",
    );
    assert!(
        parsed["note"]
            .as_str()
            .expect("note string")
            .contains("blob:hex:<hex>"),
        "note must document blob cell encoding: {parsed}",
    );
}

#[test]
fn execute_sql_decode_options_materialize_only_requested_rows() {
    let mut params = execute_sql_params("SELECT n FROM nums");

    assert_eq!(
        execute_sql_decode_options(&params).expect("valid params"),
        crate::query::DecodeQueryOptions { max_rows: None }
    );

    params.head = Some(5);
    assert_eq!(
        execute_sql_decode_options(&params).expect("valid head"),
        crate::query::DecodeQueryOptions { max_rows: Some(5) }
    );

    params.head = Some((MAX_ROWS + 99) as u32);
    assert_eq!(
        execute_sql_decode_options(&params).expect("oversized head clamps"),
        crate::query::DecodeQueryOptions {
            max_rows: Some(MAX_ROWS)
        }
    );

    params.head = None;
    params.summary = true;
    assert_eq!(
        execute_sql_decode_options(&params).expect("valid summary"),
        crate::query::DecodeQueryOptions {
            max_rows: Some(DEFAULT_EXECUTE_SQL_SUMMARY_ROWS)
        }
    );

    params.summary = false;
    params.columns_only = true;
    assert_eq!(
        execute_sql_decode_options(&params).expect("valid columns_only"),
        crate::query::DecodeQueryOptions { max_rows: Some(0) }
    );
}

#[test]
fn execute_sql_decoded_response_uses_exact_count_from_limited_decoder() {
    let decoded = crate::query::DecodedQueryResult {
        table: decoded_table(&["n"], vec![vec![json!(1)]]),
        row_count: 3,
        row_count_known: true,
        rows_truncated: true,
    };
    let mut params = execute_sql_params("SELECT n FROM nums");
    params.head = Some(1);

    let response = format_execute_sql_decoded_response_with_redaction(decoded, &params, false)
        .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(parsed["rows"], json!([[1]]));
    assert_eq!(parsed["row_count"], json!(3));
    assert_eq!(parsed["returned_rows"], json!(1));
    assert_eq!(parsed["truncated"], json!(true));
    assert_eq!(parsed["row_count_known"], json!(true));
}

#[test]
fn execute_sql_output_shape_clamps_oversized_head_and_limit_to_max_rows() {
    let mut params = execute_sql_params("SELECT n FROM nums");
    params.head = Some((MAX_ROWS + 99) as u32);
    assert_eq!(
        execute_sql_output_shape(&params, false)
            .expect("oversized head must clamp")
            .mode,
        ExecuteSqlOutputMode::LimitedRows(MAX_ROWS)
    );

    let mut params = execute_sql_params("SELECT n FROM nums");
    params.summary = true;
    params.limit = Some((MAX_ROWS + 99) as u32);
    assert_eq!(
        execute_sql_output_shape(&params, false)
            .expect("oversized summary limit must clamp")
            .mode,
        ExecuteSqlOutputMode::Summary(MAX_ROWS)
    );
}

#[test]
fn execute_sql_columns_only_can_return_count_without_row_values() {
    let decoded = crate::query::DecodedQueryResult {
        table: decoded_table(&["a", "b"], vec![]),
        row_count: 42,
        row_count_known: true,
        rows_truncated: true,
    };
    let mut params = execute_sql_params("SELECT a, b FROM t");
    params.columns_only = true;

    let response = format_execute_sql_decoded_response_with_redaction(decoded, &params, false)
        .expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(parsed["columns"], json!(["a", "b"]));
    assert_eq!(parsed["row_count"], json!(42));
    assert_eq!(parsed["returned_rows"], json!(0));
    assert_eq!(parsed["row_count_known"], json!(true));
    assert!(parsed.get("rows").is_none(), "columns_only must omit rows");
}

#[test]
fn execute_sql_summary_uses_default_sample_rows() {
    let rows = (0..12).map(|n| vec![json!(n)]).collect();
    let table = decoded_table(&["n"], rows);
    let mut params = execute_sql_params("SELECT n FROM nums");
    params.summary = true;

    let response =
        format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_json_key_order(&response, "\"truncated\":", "\"sample_rows\":");
    assert_eq!(
        parsed["sample_rows"].as_array().expect("sample rows").len(),
        DEFAULT_EXECUTE_SQL_SUMMARY_ROWS,
    );
    assert_eq!(parsed["row_count"], json!(12));
    assert_eq!(parsed["returned_rows"], json!(10));
    assert_eq!(parsed["truncated"], json!(true));
    assert!(
        parsed.get("rows").is_none(),
        "summary must not emit full rows"
    );
}

#[test]
fn execute_sql_columns_only_omits_rows() {
    let table = decoded_table(&["a", "b"], vec![vec![json!(1), json!("x")]]);
    let mut params = execute_sql_params("SELECT 1 AS a, 'x' AS b");
    params.columns_only = true;

    let response =
        format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(parsed["columns"], json!(["a", "b"]));
    assert_eq!(parsed["row_count"], json!(1));
    assert_eq!(parsed["returned_rows"], json!(0));
    assert!(parsed.get("rows").is_none(), "columns_only must omit rows");
    assert!(
        parsed.get("sample_rows").is_none(),
        "columns_only must omit sample_rows"
    );
}

#[test]
fn execute_sql_max_string_len_truncates_returned_cells() {
    let table = decoded_table(&["s"], vec![vec![json!("abcdefghijklmnopqrstuvwxyz")]]);
    let mut params = execute_sql_params("SELECT long_string AS s");
    params.max_string_len = Some(8);

    let response =
        format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(parsed["rows"][0][0], json!("abcdefgh...<truncated>"));
    assert_eq!(parsed["string_truncated"], json!(true));
    assert_eq!(parsed["redacted"], json!(false));
}

#[test]
fn execute_sql_max_string_len_truncates_blob_cells_with_type_context() {
    let table = decoded_table(&["payload"], vec![vec![json!("blob:hex:00abff102030")]]);
    let mut params = execute_sql_params("SELECT payload");
    params.max_string_len = Some(13);

    let response =
        format_execute_sql_response_with_redaction(table, &params, false).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(parsed["rows"][0][0], json!("blob:hex:00ab...(+4 bytes)"));
    assert_eq!(parsed["string_truncated"], json!(true));
    assert!(
        parsed["note"]
            .as_str()
            .expect("note")
            .contains("truncated blobs keep the prefix"),
        "note must explain blob truncation: {parsed}",
    );
}

#[test]
fn execute_sql_redact_strings_masks_common_sensitive_values() {
    let table = decoded_table(
        &["header", "path", "url"],
        vec![vec![
            json!("Authorization: Bearer secret\r\nUser-Agent: test"),
            json!("C:\\Users\\FanTao\\AppData\\Local\\Qianwen"),
            json!("https://example.test/?access_token=secret-token&ok=1"),
        ]],
    );
    let params = execute_sql_params("SELECT sensitive_cells");

    let response =
        format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(
        parsed["rows"][0][0],
        json!("Authorization: <redacted>\r\nUser-Agent: test")
    );
    assert_eq!(
        parsed["rows"][0][1],
        json!("C:\\Users\\<user>\\AppData\\Local\\Qianwen")
    );
    let redacted_url = parsed["rows"][0][2].as_str().expect("redacted URL");
    assert!(
        redacted_url.starts_with("https://example.test/?access_token=<redacted:")
            && redacted_url.ends_with(">&ok=1"),
        "access_token must be redacted with a stable placeholder: {redacted_url}",
    );
    assert_eq!(parsed["redacted"], json!(true));
}

#[test]
fn execute_sql_redact_strings_handles_token_at_eof() {
    let table = decoded_table(
        &["url"],
        vec![vec![json!(
            "https://example.test/?access_token=secret-token"
        )]],
    );
    let params = execute_sql_params("SELECT url");

    let response =
        format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    let redacted_url = parsed["rows"][0][0].as_str().expect("redacted URL");
    assert!(
        redacted_url.starts_with("https://example.test/?access_token=<redacted:")
            && redacted_url.ends_with('>'),
        "access_token at EOF must be redacted with a stable placeholder: {redacted_url}",
    );
    assert_eq!(parsed["redacted"], json!(true));
}

#[test]
fn execute_sql_redact_strings_masks_query_signatures_and_encoded_values() {
    let table = decoded_table(
            &["top_level", "encoded_nested"],
            vec![vec![
                json!(
                    "https://px.effirst.com/api/v1/jconfig?wpk-header=app%3Dueocxfzk%26ud%3Duser-42%26sign%3Dabc123&ok=1"
                ),
                json!("payload=app%3Ddemo%26sign%3Dabc123%26ud%3Duser-42%26safe%3Dkeep"),
            ]],
        );
    let params = execute_sql_params("SELECT url_args");

    let response =
        format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    let top_level = parsed["rows"][0][0].as_str().expect("top-level URL");
    assert!(
        top_level.starts_with("https://px.effirst.com/api/v1/jconfig?wpk-header=<redacted:")
            && top_level.ends_with(">&ok=1"),
        "top-level wpk-header must be redacted with a stable placeholder: {top_level}",
    );
    let nested = parsed["rows"][0][1].as_str().expect("encoded nested args");
    assert!(
        nested.starts_with("payload=app%3Ddemo%26sign%3D<redacted:")
            && nested.contains(">%26ud%3D<redacted:")
            && nested.ends_with(">%26safe%3Dkeep"),
        "encoded sensitive assignments must preserve structure and stable placeholders: {nested}",
    );
    assert_eq!(parsed["redacted"], json!(true));
}

#[test]
fn execute_sql_redact_strings_respects_query_key_boundaries() {
    let table = decoded_table(
        &["false_positive", "encoded_false_positive", "mixed"],
        vec![vec![
            json!("https://example.test/?design=dark&cloud=prod&guid=abc"),
            json!("prefixéésign%3Dabc"),
            json!("https://example.test/?design=dark&sign=real&cloud=prod&uid=user-42"),
        ]],
    );
    let params = execute_sql_params("SELECT url_args");

    let response =
        format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(
        parsed["rows"][0][0],
        json!("https://example.test/?design=dark&cloud=prod&guid=abc")
    );
    assert_eq!(parsed["rows"][0][1], json!("prefixéésign%3Dabc"));
    let mixed = parsed["rows"][0][2].as_str().expect("mixed URL");
    assert!(
        mixed.starts_with("https://example.test/?design=dark&sign=<redacted:")
            && mixed.contains(">&cloud=prod&uid=<redacted:")
            && mixed.ends_with('>'),
        "real sensitive keys must redact without collapsing all values: {mixed}",
    );
    assert_eq!(parsed["redacted"], json!(true));
}

#[test]
fn execute_sql_redact_strings_does_not_rewrite_network_url_paths_as_user_paths() {
    let table = decoded_table(
        &["cdn_url", "local_path", "file_url"],
        vec![vec![
            json!("https://cdn.example.test/Users/avatars/42.png"),
            json!("/Users/alice/trace.pftrace"),
            json!("file:///Users/alice/trace.pftrace"),
        ]],
    );
    let params = execute_sql_params("SELECT paths");

    let response =
        format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(
        parsed["rows"][0][0],
        json!("https://cdn.example.test/Users/avatars/42.png")
    );
    assert_eq!(parsed["rows"][0][1], json!("/Users/<user>/trace.pftrace"));
    assert_eq!(
        parsed["rows"][0][2],
        json!("file:///Users/<user>/trace.pftrace")
    );
    assert_eq!(parsed["redacted"], json!(true));
}

#[test]
fn execute_sql_redact_strings_preserves_low_risk_diagnostic_token_values() {
    let table = decoded_table(
        &["url"],
        vec![vec![json!(
            "https://example.test/frame?token=main_frame&session=warm&access_token=secret"
        )]],
    );
    let params = execute_sql_params("SELECT url");

    let response =
        format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");
    let url = parsed["rows"][0][0].as_str().expect("redacted URL");

    assert!(
        url.contains("token=main_frame") && url.contains("session=warm"),
        "low-risk diagnostic enum values should remain visible: {url}",
    );
    assert!(
        url.contains("access_token=<redacted:"),
        "high-risk token fields must still redact: {url}",
    );
    assert_eq!(parsed["redacted"], json!(true));
}

#[test]
fn execute_sql_redact_strings_does_not_treat_suffix_shapes_as_safe_secrets() {
    let table = decoded_table(
        &["url"],
        vec![vec![json!(
            "https://example.test/?token=evil_worker&session=cold_process&sign=main_frame"
        )]],
    );
    let params = execute_sql_params("SELECT url");

    let response =
        format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");
    let url = parsed["rows"][0][0].as_str().expect("redacted URL");

    assert!(
        url.contains("token=<redacted:")
            && url.contains("session=<redacted:")
            && url.contains("sign=<redacted:"),
        "suffix-shaped sensitive values must not bypass redaction: {url}",
    );
    assert!(
        !url.contains("evil_worker")
            && !url.contains("cold_process")
            && !url.contains("sign=main_frame"),
        "redaction must remove original sensitive values: {url}",
    );
}

#[test]
fn execute_sql_redact_strings_masks_terminal_user_profile_paths() {
    let table = decoded_table(
        &["win", "win_slash", "mac", "linux"],
        vec![vec![
            json!("C:\\Users\\Alice"),
            json!("C:/Users/Alice"),
            json!("/Users/Alice"),
            json!("/home/alice"),
        ]],
    );
    let params = execute_sql_params("SELECT profile_paths");

    let response =
        format_execute_sql_response_with_redaction(table, &params, true).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&response).expect("json");

    assert_eq!(parsed["rows"][0][0], json!("C:\\Users\\<user>"));
    assert_eq!(parsed["rows"][0][1], json!("C:/Users/<user>"));
    assert_eq!(parsed["rows"][0][2], json!("/Users/<user>"));
    assert_eq!(parsed["rows"][0][3], json!("/home/<user>"));
    assert_eq!(parsed["redacted"], json!(true));
}

#[test]
fn execute_sql_output_shape_rejects_conflicting_or_zero_limits() {
    let mut params = execute_sql_params("SELECT 1");
    params.head = Some(1);
    params.limit = Some(1);
    let err = execute_sql_output_shape(&params, false).expect_err("head + limit must reject");
    assert!(err.contains("head"), "got: {err}");

    let mut params = execute_sql_params("SELECT 1");
    params.limit = Some(0);
    let err = execute_sql_output_shape(&params, false).expect_err("limit=0 must reject");
    assert!(err.contains("> 0"), "got: {err}");

    let mut params = execute_sql_params("SELECT 1");
    params.max_string_len = Some(0);
    let err = execute_sql_output_shape(&params, false).expect_err("max_string_len=0 must reject");
    assert!(err.contains("max_string_len"), "got: {err}");
}

#[test]
fn execute_sql_params_accept_stringified_numeric_shaping_fields() {
    let p: ExecuteSqlParams =
        serde_json::from_str(r#"{"sql": "SELECT 1", "head": "3", "max_string_len": "40"}"#)
            .expect("stringified numeric shaping fields must deserialize");
    assert_eq!(p.head, Some(3));
    assert_eq!(p.max_string_len, Some(40));
}

#[test]
fn execute_sql_rejects_redaction_as_tool_parameter() {
    let err =
        serde_json::from_str::<ExecuteSqlParams>(r#"{"sql": "SELECT 1", "redact_strings": true}"#)
            .expect_err("redaction must be controlled by server policy, not the LLM");
    assert!(
        err.to_string().contains("redact_strings"),
        "error must name the rejected field, got: {err}",
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

// Without these capabilities, clients skip `tools/list` / `resources/list`
// on handshake and the router still has them, but they're invisible.
#[test]
fn get_info_declares_tools_and_resources_capabilities() {
    let info = test_server().get_info();
    assert!(
        info.capabilities.tools.is_some(),
        "server must declare `tools` capability so clients call tools/list"
    );
    assert!(
        info.capabilities.resources.is_some(),
        "server must declare `resources` capability so clients can read quickrefs"
    );
}

#[test]
fn instructions_stay_short_and_route_to_quickref() {
    let info = test_server().get_info();
    let instructions = info
        .instructions
        .expect("server must ship short routing instructions");
    assert!(
        instructions.len() <= 900,
        "instructions should stay routing-sized; got {} chars: {instructions}",
        instructions.len(),
    );
    assert!(
        instructions.contains(STDLIB_QUICKREF_URI),
        "instructions must route long stdlib guidance to the quickref resource"
    );
    assert!(
        instructions.contains("list_stdlib_modules"),
        "instructions must preserve a tools-only fallback for stdlib discovery"
    );
}

#[test]
fn stdlib_quickref_resource_metadata_is_exposed() {
    let resource = stdlib_quickref_resource();
    assert_eq!(resource.uri, STDLIB_QUICKREF_URI);
    assert_eq!(
        resource.mime_type.as_deref(),
        Some(STDLIB_QUICKREF_MIME_TYPE)
    );
    assert!(
        resource.size.unwrap_or_default() as usize >= STDLIB_QUICKREF.len(),
        "resource size should describe the quickref payload"
    );
}

#[test]
fn instructions_surface_server_redaction_policy() {
    let enabled = server_instructions_for_redaction(true);
    assert!(
        enabled.contains("SQL/Chrome tool string redaction is enabled"),
        "instructions must surface enabled privacy policy: {enabled}",
    );
    assert!(
        enabled.contains(REDACT_STRINGS_DEFAULT_ENV),
        "instructions must tell users how to control the policy: {enabled}",
    );

    let disabled = server_instructions_for_redaction(false);
    assert!(
        disabled.contains("SQL/Chrome tool string redaction is disabled"),
        "instructions must surface disabled privacy policy: {disabled}",
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
            "chrome_page_load_resource_hotspots",
            "chrome_page_load_resource_pipeline",
            "chrome_page_load_resource_summary",
            "chrome_page_load_script_hotspots",
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
            "slice_descendants_breakdown",
        ],
    );
}

#[test]
fn tool_annotations_surface_safety_hints() {
    let server = test_server();
    for tool in server.tool_router.list_all() {
        let annotations = tool
            .annotations
            .as_ref()
            .unwrap_or_else(|| panic!("tool `{}` must carry annotations", tool.name));
        assert_eq!(
            annotations.open_world_hint,
            Some(false),
            "tool `{}` should advertise a closed-world trace-analysis domain",
            tool.name,
        );
        assert_eq!(
            annotations.destructive_hint,
            Some(false),
            "tool `{}` should advertise non-destructive behavior",
            tool.name,
        );
        assert_eq!(
            annotations.idempotent_hint,
            Some(true),
            "tool `{}` should advertise idempotence for repeated calls",
            tool.name,
        );
        if tool.name == "load_trace" {
            assert_eq!(
                annotations.read_only_hint, None,
                "load_trace changes server current-trace state, so it should not claim read-only"
            );
        } else {
            assert_eq!(
                annotations.read_only_hint,
                Some(true),
                "tool `{}` should advertise read-only behavior",
                tool.name,
            );
        }
    }
}

#[test]
fn tool_descriptions_stay_within_context_budget() {
    let server = test_server();
    let tools = server.tool_router.list_all();
    let total: usize = tools
        .iter()
        .map(|tool| tool.description.as_deref().unwrap_or("").len())
        .sum();
    assert!(
        total <= 15_000,
        "tool descriptions should stay routing-sized; total={total}"
    );
    for tool in tools {
        let len = tool.description.as_deref().unwrap_or("").len();
        assert!(
            len <= 2_500,
            "tool `{}` description is too large for default tools/list context: {len}",
            tool.name,
        );
    }
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
    let json: serde_json::Value =
        serde_json::from_str(STDLIB_MODULE_LIST).expect("STDLIB_MODULE_LIST must be valid JSON");
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
            module["views"].as_array().is_some() && !module["views"].as_array().unwrap().is_empty(),
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

#[test]
fn list_stdlib_modules_filters_by_domain_query_and_limit() {
    let response = filtered_stdlib_modules_json(&ListStdlibModulesParams {
        domain: Some("chrome".to_owned()),
        query: Some("jank".to_owned()),
        limit: Some(2),
    })
    .expect("filters should serialize");
    let modules: Vec<serde_json::Value> = serde_json::from_str(&response).unwrap();

    assert_eq!(modules.len(), 1);
    assert_eq!(
        modules[0]["module"].as_str(),
        Some("chrome.scroll_jank.scroll_jank_v3")
    );
}

#[test]
fn list_stdlib_modules_rejects_invalid_filter_values() {
    let err = filtered_stdlib_modules_json(&ListStdlibModulesParams {
        domain: Some("ios".to_owned()),
        query: None,
        limit: None,
    })
    .expect_err("unknown domains must be rejected");
    assert!(err.contains("domain"), "got: {err}");

    let err = filtered_stdlib_modules_json(&ListStdlibModulesParams {
        domain: None,
        query: None,
        limit: Some(0),
    })
    .expect_err("zero limit must be rejected");
    assert!(err.contains("limit"), "got: {err}");
}

/// Integration test exercising the full wrapper path of EVERY chrome_*
/// handler: client_for → ensure_chrome_trace → error-on-non-chrome.
/// Guards against regressions where any future handler forgets to call
/// the preflight (SQL-level e2e wouldn't catch that). The calls share
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
            .chrome_scroll_jank_summary(Parameters(ChromeTraceParams {
                limit: None,
                max_string_len: None,
            }))
            .await;
        let err = r
            .map(|_| ())
            .expect_err("chrome_scroll_jank_summary: preflight must reject");
        assert!(err.contains("Chrome scroll jank summary"), "got: {err}");
        assert!(err.contains("Chrome-family trace"), "got: {err}");
        assert!(err.contains("list_stdlib_modules"), "got: {err}");

        let r = server
            .chrome_page_load_summary(Parameters(ChromeTraceParams {
                limit: None,
                max_string_len: None,
            }))
            .await;
        let err = r
            .map(|_| ())
            .expect_err("chrome_page_load_summary: preflight must reject");
        assert!(err.contains("Chrome page load summary"), "got: {err}");
        assert!(err.contains("Chrome-family trace"), "got: {err}");
        assert!(err.contains("list_stdlib_modules"), "got: {err}");

        let r = server
            .chrome_page_load_resource_hotspots(Parameters(ChromePageLoadResourceHotspotsParams {
                page_load_id: None,
                navigation_id: None,
                phase: None,
                start_ts_ns: None,
                end_ts_ns: None,
                min_dur_ms: None,
                limit: None,
                max_string_len: None,
            }))
            .await;
        let err = r
            .map(|_| ())
            .expect_err("chrome_page_load_resource_hotspots: preflight must reject");
        assert!(
            err.contains("Chrome page-load resource hotspots"),
            "got: {err}"
        );
        assert!(err.contains("Chrome-family trace"), "got: {err}");
        assert!(err.contains("list_stdlib_modules"), "got: {err}");

        let r = server
            .chrome_page_load_resource_summary(Parameters(ChromePageLoadResourceSummaryParams {
                page_load_id: None,
                navigation_id: None,
                phase: None,
                start_ts_ns: None,
                end_ts_ns: None,
                min_overlap_ms: None,
                url_grouping: None,
                limit: None,
                max_string_len: None,
            }))
            .await;
        let err = r
            .map(|_| ())
            .expect_err("chrome_page_load_resource_summary: preflight must reject");
        assert!(
            err.contains("Chrome page-load resource summary"),
            "got: {err}"
        );
        assert!(err.contains("Chrome-family trace"), "got: {err}");
        assert!(err.contains("list_stdlib_modules"), "got: {err}");

        let r = server
            .chrome_page_load_resource_pipeline(Parameters(ChromePageLoadResourcePipelineParams {
                url_substring: Some("main.js".to_owned()),
                example_slice_id: None,
                page_load_id: None,
                navigation_id: None,
                phase: None,
                start_ts_ns: None,
                end_ts_ns: None,
                url_grouping: None,
                limit: None,
                max_string_len: None,
            }))
            .await;
        let err = r
            .map(|_| ())
            .expect_err("chrome_page_load_resource_pipeline: preflight must reject");
        assert!(
            err.contains("Chrome page-load resource pipeline"),
            "got: {err}"
        );
        assert!(err.contains("Chrome-family trace"), "got: {err}");
        assert!(err.contains("list_stdlib_modules"), "got: {err}");

        let r = server
            .chrome_page_load_script_hotspots(Parameters(ChromePageLoadScriptHotspotsParams {
                process_name: None,
                pid: None,
                upid: None,
                page_load_id: None,
                navigation_id: None,
                phase: None,
                start_ts_ns: None,
                end_ts_ns: None,
                min_total_ms: None,
                limit: None,
                max_string_len: None,
            }))
            .await;
        let err = r
            .map(|_| ())
            .expect_err("chrome_page_load_script_hotspots: preflight must reject");
        assert!(
            err.contains("Chrome page-load script hotspots"),
            "got: {err}"
        );
        assert!(err.contains("Chrome-family trace"), "got: {err}");
        assert!(err.contains("list_stdlib_modules"), "got: {err}");

        let r = server
            .chrome_main_thread_hotspots(Parameters(ChromeMainThreadHotspotsParams {
                process_name: None,
                pid: None,
                upid: None,
                page_load_id: None,
                navigation_id: None,
                phase: None,
                start_ts_ns: None,
                end_ts_ns: None,
                min_dur_ms: None,
                limit: None,
                max_string_len: None,
            }))
            .await;
        let err = r
            .map(|_| ())
            .expect_err("chrome_main_thread_hotspots: preflight must reject");
        assert!(err.contains("Chrome main-thread hotspots"), "got: {err}");
        assert!(err.contains("Chrome-family trace"), "got: {err}");
        assert!(err.contains("list_stdlib_modules"), "got: {err}");

        let r = server
            .chrome_startup_summary(Parameters(ChromeTraceParams {
                limit: None,
                max_string_len: None,
            }))
            .await;
        let err = r
            .map(|_| ())
            .expect_err("chrome_startup_summary: preflight must reject");
        assert!(err.contains("Chrome startup summary"), "got: {err}");
        assert!(err.contains("Chrome-family trace"), "got: {err}");
        assert!(err.contains("list_stdlib_modules"), "got: {err}");

        let r = server
            .chrome_web_content_interactions(Parameters(ChromeTraceParams {
                limit: None,
                max_string_len: None,
            }))
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

#[test]
fn list_tables_response_is_not_contaminated_by_previous_tool_call() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = Arc::new(TraceProcessorManager::new_with_starting_port(1, 19_031));
        let server = PerfettoMcpServer::new(manager);
        server
            .load_trace(Parameters(LoadTraceParams {
                path: "tests/fixtures/page_loads.pftrace".to_owned(),
            }))
            .await
            .expect("load_trace must succeed");

        let page_load_response = server
            .chrome_page_load_summary(Parameters(ChromeTraceParams {
                limit: None,
                max_string_len: None,
            }))
            .await
            .expect("page-load summary must succeed");
        assert!(
            page_load_response.contains("\"columns\""),
            "sanity-check prior chrome tool response shape: {page_load_response}",
        );

        let list_tables_response = server
            .list_tables(Parameters(ListTablesParams {
                pattern: Some("chrome*".to_owned()),
            }))
            .await
            .expect("list_tables must succeed after a chrome tool call");
        let parsed: serde_json::Value =
            serde_json::from_str(&list_tables_response).expect("valid JSON");
        assert!(
            parsed.get("names").and_then(|v| v.as_array()).is_some(),
            "list_tables must return its own names shape, got: {list_tables_response}",
        );
        assert!(
            parsed.get("columns").is_none(),
            "list_tables must not return a previous table-shaped response: {list_tables_response}",
        );
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

#[test]
fn execute_sql_schema_does_not_expose_redaction_control() {
    let server = test_server();
    let tool = server
        .tool_router
        .list_all()
        .into_iter()
        .find(|t| t.name == "execute_sql")
        .expect("execute_sql tool must exist");
    let schema = serde_json::to_string(&tool.input_schema).expect("input schema must serialize");
    assert!(
        !schema.contains("redact_strings"),
        "redaction is server-side policy and must not be exposed to the LLM: {schema}",
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
    let p: ChromeMainThreadHotspotsParams = serde_json::from_str(
            r#"{"pid": "12800", "page_load_id": "1", "start_ts_ns": "100", "end_ts_ns": "200", "min_dur_ms": "50", "limit": "30", "max_string_len": "260"}"#,
        )
        .expect("stringified numerics must deserialize after v0.11.3");
    assert_eq!(p.pid, Some(12800));
    assert_eq!(p.page_load_id, Some(1));
    assert_eq!(p.start_ts_ns, Some(100));
    assert_eq!(p.end_ts_ns, Some(200));
    assert_eq!(p.min_dur_ms, Some(50.0));
    assert_eq!(p.limit, Some(30));
    assert_eq!(p.max_string_len, Some(260));
}

#[test]
fn chrome_page_load_resource_hotspots_params_accepts_stringified_numerics() {
    let p: ChromePageLoadResourceHotspotsParams = serde_json::from_str(
            r#"{"navigation_id": "7", "start_ts_ns": "100", "end_ts_ns": "200", "min_dur_ms": "50", "limit": "30", "max_string_len": "260"}"#,
        )
        .expect("stringified numerics must deserialize");
    assert_eq!(p.navigation_id, Some(7));
    assert_eq!(p.start_ts_ns, Some(100));
    assert_eq!(p.end_ts_ns, Some(200));
    assert_eq!(p.min_dur_ms, Some(50.0));
    assert_eq!(p.limit, Some(30));
    assert_eq!(p.max_string_len, Some(260));
}

#[test]
fn chrome_page_load_resource_summary_params_accepts_stringified_numerics() {
    let p: ChromePageLoadResourceSummaryParams = serde_json::from_str(
            r#"{"navigation_id": "7", "start_ts_ns": "100", "end_ts_ns": "200", "min_overlap_ms": "50", "url_grouping": "without_query", "limit": "30", "max_string_len": "260"}"#,
        )
        .expect("stringified numerics must deserialize");
    assert_eq!(p.navigation_id, Some(7));
    assert_eq!(p.start_ts_ns, Some(100));
    assert_eq!(p.end_ts_ns, Some(200));
    assert_eq!(p.min_overlap_ms, Some(50.0));
    assert_eq!(
        p.url_grouping,
        Some(ChromePageLoadResourceUrlGrouping::WithoutQuery)
    );
    assert_eq!(p.limit, Some(30));
    assert_eq!(p.max_string_len, Some(260));
}

#[test]
fn chrome_page_load_resource_pipeline_params_accepts_stringified_numerics() {
    let p: ChromePageLoadResourcePipelineParams = serde_json::from_str(
            r#"{"url_substring": "main.js", "example_slice_id": "123", "navigation_id": "7", "start_ts_ns": "100", "end_ts_ns": "200", "url_grouping": "without_query", "limit": "30", "max_string_len": "260"}"#,
        )
        .expect("stringified numerics must deserialize");
    assert_eq!(p.url_substring.as_deref(), Some("main.js"));
    assert_eq!(p.example_slice_id, Some(123));
    assert_eq!(p.navigation_id, Some(7));
    assert_eq!(p.start_ts_ns, Some(100));
    assert_eq!(p.end_ts_ns, Some(200));
    assert_eq!(
        p.url_grouping,
        Some(ChromePageLoadResourceUrlGrouping::WithoutQuery)
    );
    assert_eq!(p.limit, Some(30));
    assert_eq!(p.max_string_len, Some(260));
}

#[test]
fn chrome_page_load_script_hotspots_params_accepts_stringified_numerics() {
    let p: ChromePageLoadScriptHotspotsParams = serde_json::from_str(
            r#"{"upid": "14", "navigation_id": "7", "start_ts_ns": "100", "end_ts_ns": "200", "min_total_ms": "20", "limit": "30", "max_string_len": "260"}"#,
        )
        .expect("stringified numerics must deserialize");
    assert_eq!(p.upid, Some(14));
    assert_eq!(p.navigation_id, Some(7));
    assert_eq!(p.start_ts_ns, Some(100));
    assert_eq!(p.end_ts_ns, Some(200));
    assert_eq!(p.min_total_ms, Some(20.0));
    assert_eq!(p.limit, Some(30));
    assert_eq!(p.max_string_len, Some(260));
}

#[test]
fn chrome_main_thread_hotspots_params_accept_navigation_id() {
    let p: ChromeMainThreadHotspotsParams =
        serde_json::from_str(r#"{"navigation_id": "7", "phase": "dcl_to_fcp"}"#)
            .expect("navigation_id and phase must deserialize");
    assert_eq!(p.page_load_id, None);
    assert_eq!(p.navigation_id, Some(7));
    assert_eq!(p.phase, Some(ChromeMainThreadHotspotsPhase::DclToFcp));
}

#[test]
fn chrome_trace_params_accept_stringified_numerics() {
    let p: ChromeTraceParams = serde_json::from_str(r#"{"limit": "25", "max_string_len": "300"}"#)
        .expect("stringified Chrome params must deserialize");
    assert_eq!(p.limit, Some(25));
    assert_eq!(p.max_string_len, Some(300));
}

/// JsonSchema must still advertise strict types so well-behaved LLMs
/// don't see "string-or-integer" weirdness on `tools/list`. The
/// `deserialize_with` is server-side leniency only, invisible to the
/// schema. Pin this against the actual `tools/list` payload for
/// `chrome_main_thread_hotspots`.
#[test]
fn schema_for_chrome_hotspots_advertises_strict_numeric_types() {
    let server = test_server();
    for (tool_name, strict_pairs) in [
        (
            "chrome_main_thread_hotspots",
            vec![
                ("pid", "integer"),
                ("upid", "integer"),
                ("page_load_id", "integer"),
                ("navigation_id", "integer"),
                ("start_ts_ns", "integer"),
                ("end_ts_ns", "integer"),
                ("min_dur_ms", "number"),
                ("limit", "integer"),
                ("max_string_len", "integer"),
            ],
        ),
        (
            "chrome_page_load_resource_hotspots",
            vec![
                ("page_load_id", "integer"),
                ("navigation_id", "integer"),
                ("start_ts_ns", "integer"),
                ("end_ts_ns", "integer"),
                ("min_dur_ms", "number"),
                ("limit", "integer"),
                ("max_string_len", "integer"),
            ],
        ),
        (
            "chrome_page_load_resource_summary",
            vec![
                ("page_load_id", "integer"),
                ("navigation_id", "integer"),
                ("start_ts_ns", "integer"),
                ("end_ts_ns", "integer"),
                ("min_overlap_ms", "number"),
                ("limit", "integer"),
                ("max_string_len", "integer"),
            ],
        ),
        (
            "chrome_page_load_resource_pipeline",
            vec![
                ("example_slice_id", "integer"),
                ("page_load_id", "integer"),
                ("navigation_id", "integer"),
                ("start_ts_ns", "integer"),
                ("end_ts_ns", "integer"),
                ("limit", "integer"),
                ("max_string_len", "integer"),
            ],
        ),
        (
            "chrome_page_load_script_hotspots",
            vec![
                ("pid", "integer"),
                ("upid", "integer"),
                ("page_load_id", "integer"),
                ("navigation_id", "integer"),
                ("start_ts_ns", "integer"),
                ("end_ts_ns", "integer"),
                ("min_total_ms", "number"),
                ("limit", "integer"),
                ("max_string_len", "integer"),
            ],
        ),
    ] {
        let tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|t| t.name == tool_name)
            .unwrap_or_else(|| panic!("{tool_name} tool must exist"));
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("input schema must have a `properties` object");
        // Each numeric field must advertise its simple type — never a union
        // with "string", never an `anyOf`. The lenient deserializer accepts
        // strings server-side; the schema is for advertising strict types
        // to well-behaved LLMs.
        for (field, expected_type) in strict_pairs {
            let prop = props
                .get(field)
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
                serde_json::Value::String(s) => s == expected_type,
                serde_json::Value::Array(arr) => {
                    arr.iter().any(|v| v.as_str() == Some(expected_type))
                }
                _ => false,
            };
            assert!(
                advertises_expected,
                "`{field}` schema must advertise `{expected_type}`: {prop}",
            );
        }
    }
}

// -- v0.11.3 `name` alias on table_name ------------------------------

#[test]
fn list_table_structure_accepts_name_alias() {
    let from_canonical: TableStructureParams = serde_json::from_str(r#"{"table_name": "slice"}"#)
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
    let r = serde_json::from_str::<ExecuteSqlParams>(r#"{"trace_path": "/x", "sql": "SELECT 1"}"#);
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
/// prone to hallucinate fields. The advertised tools all carry params with
/// `deny_unknown_fields`; rmcp's parameterless `async fn foo(&self)` shape,
/// by contrast, emits an *open* schema and silently ignores hallucinated fields.
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

/// No-filter SQL keeps the same `LEFT JOIN process p ON ct.upid = p.upid` clause
/// as the filtered variants, so the join is harmless when no pid filter is
/// set — this means handlers can always use the same builder.
#[test]
fn chrome_main_thread_hotspots_sql_no_filter_runs_all_main_threads() {
    let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters::default())
        .expect("builder must succeed");
    assert!(sql.contains("ct.ts"));
    assert!(sql.contains("ct.upid"));
    assert!(sql.contains("p.pid"));
    assert!(sql.contains("LEFT JOIN thread t ON ct.utid = t.utid"));
    assert!(sql.contains("LEFT JOIN process p ON ct.upid = p.upid"));
    assert!(sql.contains("WHERE (t.is_main_thread = 1 OR ct.thread_name GLOB 'Cr*Main')"));
    assert!(sql.contains("AND ct.dur > 16000000"));
    assert!(
        sql.contains("ct.dur / 1e6 AS dur_ms")
            && sql.contains("ROUND(ct.dur / 1e6, 3) AS overlap_dur_ms"),
        "no-window overlap duration should equal full task duration, got: {sql}",
    );
    assert!(sql.contains("ORDER BY ct.dur DESC LIMIT 100"));
    assert!(
        !sql.contains("chrome.page_loads"),
        "no-filter SQL must not include page-load window CTE, got: {sql}",
    );
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
fn chrome_main_thread_hotspots_sql_uses_name_fallback_for_chromium_traces() {
    let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters::default())
        .expect("builder must succeed");
    assert!(
        sql.contains("ct.thread_name GLOB 'Cr*Main'"),
        "main-thread detection must fall back to Chrome thread names, got: {sql}",
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

#[test]
fn chrome_main_thread_hotspots_sql_with_raw_window_emits_ts_filters() {
    let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
        start_ts_ns: Some(1000),
        end_ts_ns: Some(2000),
        ..Default::default()
    })
    .expect("raw-window builder must succeed");
    assert!(sql.contains("MAX(ct.ts, 1000)"), "got: {sql}");
    assert!(sql.contains("MIN(ct.ts + ct.dur, 2000)"), "got: {sql}");
    assert!(sql.contains("AND ct.ts + ct.dur > 1000"), "got: {sql}");
    assert!(sql.contains("AND ct.ts < 2000"), "got: {sql}");
    assert!(
        sql.contains("AND (MIN(ct.ts + ct.dur, 2000) - MAX(ct.ts, 1000)) > 16000000"),
        "windowed min_dur_ms must filter clipped overlap duration, got: {sql}",
    );
    assert!(
        sql.contains("ORDER BY (MIN(ct.ts + ct.dur, 2000) - MAX(ct.ts, 1000)) DESC, ct.dur DESC"),
        "windowed hotspots must rank by clipped overlap before full duration, got: {sql}",
    );
    assert!(
        !sql.contains("hotspot_window"),
        "raw timestamp filters alone must not require page_loads, got: {sql}",
    );
}

#[test]
fn chrome_main_thread_hotspots_sql_with_page_load_defaults_to_nav_to_fcp() {
    let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
        page_load_id: Some(7),
        ..Default::default()
    })
    .expect("page-load-window builder must succeed");
    assert!(
        sql.contains("INCLUDE PERFETTO MODULE chrome.page_loads;"),
        "got: {sql}"
    );
    assert!(
        sql.contains("navigation_start_ts AS start_ts") && sql.contains("fcp_ts AS end_ts"),
        "page_load_id without phase must default to navigation_to_fcp, got: {sql}",
    );
    assert!(
        sql.contains("WHERE id = 7 "),
        "page_load_id must match only chrome_page_loads.id, got: {sql}",
    );
    assert!(
        !sql.contains("navigation_id = 7"),
        "page_load_id must not also match navigation_id, got: {sql}",
    );
    assert!(sql.contains("CROSS JOIN hotspot_window hw"), "got: {sql}");
    assert!(sql.contains("MAX(ct.ts, hw.start_ts)"), "got: {sql}");
    assert!(sql.contains("MIN(ct.ts + ct.dur, hw.end_ts)"), "got: {sql}");
    assert!(
        sql.contains("AND ct.ts + ct.dur > hw.start_ts"),
        "got: {sql}"
    );
    assert!(sql.contains("AND ct.ts < hw.end_ts"), "got: {sql}");
    assert!(
        sql.contains(
            "ORDER BY (MIN(ct.ts + ct.dur, hw.end_ts) - MAX(ct.ts, hw.start_ts)) DESC, ct.dur DESC"
        ),
        "page-load windows must rank by clipped overlap, got: {sql}",
    );
}

#[test]
fn chrome_main_thread_hotspots_sql_with_navigation_id_matches_navigation_only() {
    let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
        navigation_id: Some(7),
        ..Default::default()
    })
    .expect("navigation-window builder must succeed");
    assert!(
        sql.contains("navigation_start_ts AS start_ts") && sql.contains("fcp_ts AS end_ts"),
        "navigation_id without phase must default to navigation_to_fcp, got: {sql}",
    );
    assert!(
        sql.contains("WHERE navigation_id = 7 "),
        "navigation_id must match only chrome_page_loads.navigation_id, got: {sql}",
    );
    assert!(
        !sql.contains("WHERE id = 7"),
        "navigation_id must not also match page_loads.id, got: {sql}",
    );
}

#[test]
fn chrome_main_thread_hotspots_sql_with_phase_without_id_uses_latest_page_load() {
    let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
        phase: Some(ChromeMainThreadHotspotsPhase::DclToFcp),
        ..Default::default()
    })
    .expect("phase-only builder must succeed");
    assert!(
        sql.contains("dom_content_loaded_event_ts AS start_ts") && sql.contains("fcp_ts AS end_ts"),
        "dcl_to_fcp phase must select DCL→FCP window, got: {sql}",
    );
    assert!(
        !sql.contains("WHERE id ="),
        "phase without page_load_id should use latest page load, got: {sql}",
    );
    assert!(
        sql.contains("ORDER BY navigation_start_ts DESC LIMIT 1"),
        "phase-only SQL must use latest page load deterministically, got: {sql}",
    );
}

#[test]
fn chrome_main_thread_hotspots_sql_rejects_invalid_windows() {
    let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
        page_load_id: Some(-1),
        ..Default::default()
    })
    .expect_err("negative page_load_id must error");
    assert!(err.to_string().contains("page_load_id"), "got: {err}");

    let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
        navigation_id: Some(-1),
        ..Default::default()
    })
    .expect_err("negative navigation_id must error");
    assert!(err.to_string().contains("navigation_id"), "got: {err}");

    let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
        page_load_id: Some(1),
        navigation_id: Some(7),
        ..Default::default()
    })
    .expect_err("page_load_id plus navigation_id must error");
    assert!(err.to_string().contains("mutually exclusive"), "got: {err}");

    let err = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
        start_ts_ns: Some(2000),
        end_ts_ns: Some(1000),
        ..Default::default()
    })
    .expect_err("end before start must error");
    assert!(err.to_string().contains("end_ts_ns"), "got: {err}");
}

/// `min_dur_ms = 33.0` translates to an overlap-duration threshold. Default
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

/// `min_dur_ms = 0.0` is the explicit "show me everything" path — emits a
/// zero overlap-duration threshold so SQL still runs but only filters out zero-duration rows
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

#[test]
fn chrome_page_load_resource_hotspots_sql_defaults_to_resource_slice_scan() {
    let sql =
        chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters::default())
            .expect("resource builder must succeed");
    assert!(
        sql.contains("WITH resource_candidate_slices AS") && sql.contains("resource_candidates AS"),
        "default SQL must still use a CTE for result shaping, got: {sql}",
    );
    assert!(
        !sql.contains("chrome.page_loads"),
        "no-window resource SQL must not include page-loads, got: {sql}",
    );
    assert!(
        sql.contains("WHERE s.dur >= 50000000"),
        "default resource threshold must be 50 ms, got: {sql}",
    );
    assert!(
        sql.contains("FROM slice s LEFT JOIN track tr ON s.track_id = tr.id"),
        "resource SQL must retain non-thread-track slices before adding context, got: {sql}",
    );
    assert!(
        sql.contains("LEFT JOIN process_track pt ON s.track_id = pt.id"),
        "resource SQL must include process-track slices, got: {sql}",
    );
    assert!(
        sql.contains("resource_candidate_selected_urls") && sql.contains("MIN(rcu.url) AS url"),
        "resource SQL must collapse same-priority URL ties deterministically, got: {sql}",
    );
    assert!(
        sql.contains("LEFT JOIN process_track parent_pt ON tr.parent_id = parent_pt.id"),
        "resource SQL must include async tracks parented by process tracks, got: {sql}",
    );
    assert!(
        sql.contains("COALESCE(t.name, parent_t.name) AS thread_name"),
        "resource SQL must expose nullable thread context for async/process tracks, got: {sql}",
    );
    assert!(
        sql.contains("JOIN resource_candidate_selected_urls rcsu ON rcsu.id = rcs.id"),
        "resource tool must suppress URL-less wrapper rows through URL-priority joins, got: {sql}",
    );
    assert!(
        sql.contains("ORDER BY overlap_ms DESC, dur_ms DESC, start_ms ASC LIMIT 100"),
        "default resource limit/order must be stable, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_hotspots_sql_excludes_lifecycle_load_slices() {
    let sql =
        chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters::default())
            .expect("resource builder must succeed");
    assert!(
        !sql.contains("s.name GLOB '*Load*'"),
        "resource matching must not admit every Load-named lifecycle slice, got: {sql}",
    );
    assert!(
        sql.contains("s.name GLOB '*URLLoader*'"),
        "resource matching must keep concrete URLLoader spans, got: {sql}",
    );
    for excluded in [
        "*PageLoadMetrics*",
        "*DidCommitProvisionalLoad*",
        "*DidStartProvisionalLoad*",
        "*DidStopLoading*",
        "*DidFinishLoad*",
    ] {
        assert!(
            sql.contains(excluded),
            "resource SQL must explicitly exclude lifecycle pattern {excluded}, got: {sql}",
        );
    }
}

#[test]
fn chrome_page_load_resource_hotspots_sql_with_page_load_window_uses_overlap() {
    let sql = chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters {
        window: ChromePageLoadWindowFilters {
            page_load_id: Some(7),
            phase: Some(ChromePageLoadPhase::DclToFcp),
            ..Default::default()
        },
        min_dur_ms: Some(0.0),
        limit: Some(25),
    })
    .expect("resource window builder must succeed");
    assert!(
        sql.contains("INCLUDE PERFETTO MODULE chrome.page_loads;"),
        "windowed SQL must include page-loads, got: {sql}",
    );
    assert!(
        sql.contains("dom_content_loaded_event_ts AS start_ts, fcp_ts AS end_ts"),
        "dcl_to_fcp phase must select DCL→FCP, got: {sql}",
    );
    assert!(
        sql.contains("WHERE id = 7 "),
        "page_load_id must match only chrome_page_loads.id, got: {sql}",
    );
    assert!(
        !sql.contains("navigation_id = 7"),
        "page_load_id must not also match navigation_id, got: {sql}",
    );
    assert!(
        sql.contains("s.ts + s.dur > rw.start_ts"),
        "resource windows must include overlapping slices, got: {sql}",
    );
    assert!(
        sql.contains("s.ts < rw.end_ts"),
        "resource windows must bound overlap by end_ts, got: {sql}",
    );
    assert!(
        sql.contains("AND rw.end_ts > rw.start_ts"),
        "resource windows must reject reversed page-load phases before overlap math, got: {sql}",
    );
    assert!(
        sql.contains("pct_of_window"),
        "resource rows must expose window percentage, got: {sql}",
    );
    assert!(
        sql.contains("LIMIT 25"),
        "explicit limit must be honored, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_hotspots_sql_ands_raw_bounds_with_phase_window() {
    let sql = chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters {
        window: ChromePageLoadWindowFilters {
            page_load_id: Some(7),
            phase: Some(ChromePageLoadPhase::NavigationToFcp),
            start_ts_ns: Some(1000),
            end_ts_ns: Some(2000),
            ..Default::default()
        },
        ..Default::default()
    })
    .expect("resource window builder must succeed");
    assert!(
        sql.contains("MAX(rw.start_ts, 1000)"),
        "raw start_ts_ns must be intersected with phase start, got: {sql}",
    );
    assert!(
        sql.contains("MIN(rw.end_ts, 2000)"),
        "raw end_ts_ns must be intersected with phase end, got: {sql}",
    );
    assert!(
        sql.contains("s.ts + s.dur > MAX(rw.start_ts, 1000)"),
        "overlap lower predicate must use effective start, got: {sql}",
    );
    assert!(
        sql.contains("s.ts < MIN(rw.end_ts, 2000)"),
        "overlap upper predicate must use effective end, got: {sql}",
    );
    assert!(
        sql.contains("AND MIN(rw.end_ts, 2000) > MAX(rw.start_ts, 1000)"),
        "effective window must reject empty/reversed intersections, got: {sql}",
    );
    assert!(
        sql.contains("/ (MIN(rw.end_ts, 2000) - MAX(rw.start_ts, 1000))"),
        "pct_of_window denominator must use effective window duration, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_hotspots_sql_validates_shared_window_params() {
    let err = chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters {
        window: ChromePageLoadWindowFilters {
            page_load_id: Some(1),
            navigation_id: Some(7),
            ..Default::default()
        },
        ..Default::default()
    })
    .expect_err("page_load_id plus navigation_id must error");
    assert!(err.to_string().contains("mutually exclusive"), "got: {err}");

    let err = chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters {
        window: ChromePageLoadWindowFilters {
            start_ts_ns: Some(2000),
            end_ts_ns: Some(1000),
            ..Default::default()
        },
        ..Default::default()
    })
    .expect_err("end before start must error");
    assert!(err.to_string().contains("end_ts_ns"), "got: {err}");
}

#[test]
fn chrome_page_load_resource_summary_sql_groups_by_url() {
    let sql =
        chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters::default())
            .expect("resource summary builder must succeed");
    assert!(
        sql.contains("resource_rows AS"),
        "summary SQL must add a URL-key shaping CTE, got: {sql}",
    );
    assert!(
        sql.contains("url AS url_key"),
        "default grouping must preserve full URL, got: {sql}",
    );
    assert!(
        sql.contains("GROUP BY rr.url_key"),
        "summary SQL must aggregate by URL key, got: {sql}",
    );
    assert!(
        sql.contains("ROUND(MAX(rr.overlap_dur) / 1e6, 3) AS max_overlap_ms"),
        "primary ranking metric must use per-URL max overlap, got: {sql}",
    );
    assert!(
        sql.contains("ROUND(SUM(rr.overlap_dur) / 1e6, 3) AS summed_overlap_ms"),
        "auxiliary summed metric must be explicit, got: {sql}",
    );
    assert!(
        sql.contains("AS relation_to_navigation"),
        "summary must classify URL relatedness to the navigation URL, got: {sql}",
    );
    assert!(
        sql.contains("AS navigation_context_status")
            && sql.contains("AS navigation_match_count")
            && sql.contains("AS navigation_url"),
        "summary must expose navigation context evidence for relation labels, got: {sql}",
    );
    assert!(
        sql.contains("AS renderer_relation"),
        "summary must classify target vs other renderer involvement, got: {sql}",
    );
    assert!(
        sql.contains("AS renderer_relation_confidence")
            && sql.contains("AS renderer_relation_source"),
        "summary must expose renderer relation confidence/source, got: {sql}",
    );
    assert!(
        sql.contains("AS primary_slice_name"),
        "compact summary should expose one representative slice name, got: {sql}",
    );
    assert!(
        sql.contains("GROUP_CONCAT(DISTINCT rr.priority) AS priorities"),
        "summary must expose resource priorities when present, got: {sql}",
    );
    assert!(
        sql.contains("ORDER BY MAX(rr.overlap_dur) DESC"),
        "summary must rank by max overlap instead of summed overlap, got: {sql}",
    );
    assert!(
        sql.contains("HAVING MAX(rr.overlap_dur) >= 50000000"),
        "default min_overlap_ms must be 50 ms, got: {sql}",
    );
    assert!(
        sql.contains("LIMIT 25"),
        "default summary limit should be compact, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_summary_sql_can_group_without_query() {
    let sql = chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters {
        window: ChromePageLoadWindowFilters {
            page_load_id: Some(7),
            phase: Some(ChromePageLoadPhase::NavigationToFcp),
            start_ts_ns: Some(1000),
            end_ts_ns: Some(2000),
            ..Default::default()
        },
        min_overlap_ms: Some(0.0),
        url_grouping: Some(ChromePageLoadResourceUrlGrouping::WithoutQuery),
        limit: Some(25),
    })
    .expect("resource summary window builder must succeed");
    assert!(
        sql.contains("CASE WHEN INSTR(url, '?') > 0"),
        "without_query grouping must strip query strings, got: {sql}",
    );
    assert!(
        sql.contains("MAX(rw.start_ts, 1000)"),
        "raw start bound must AND with page-load window, got: {sql}",
    );
    assert!(
        sql.contains("MIN(rw.end_ts, 2000)"),
        "raw end bound must AND with page-load window, got: {sql}",
    );
    assert!(
        sql.contains("HAVING MAX(rr.overlap_dur) >= 0"),
        "zero min_overlap_ms should show all URL groups, got: {sql}",
    );
    assert!(
        sql.contains("LIMIT 25"),
        "explicit limit must be applied, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_summary_sql_scopes_navigation_context_to_raw_window() {
    let sql = chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters {
        window: ChromePageLoadWindowFilters {
            start_ts_ns: Some(1000),
            end_ts_ns: Some(2000),
            ..Default::default()
        },
        ..Default::default()
    })
    .expect("resource summary raw-window builder must succeed");
    assert!(
        sql.contains("navigation_start_ts < 2000"),
        "raw-window navigation context must not use latest whole-trace nav, got: {sql}",
    );
    assert!(
        sql.contains("SELECT COUNT(*) FROM chrome_page_loads WHERE"),
        "raw-window navigation context must count matching navigations, got: {sql}",
    );
    assert!(
        sql.contains("ELSE 'ambiguous'"),
        "raw-window navigation context must surface ambiguous multi-navigation windows, got: {sql}",
    );
    assert!(
        sql.contains("NULLIF(MAX(") && sql.contains("COALESCE(mark_interactive_ts, -1)"),
        "raw-window navigation context must use the latest non-null page-load marker, got: {sql}",
    );
    assert!(
            sql.contains("), -1) > 1000"),
            "raw-window navigation context must require latest marker overlap with the raw start, got: {sql}",
        );
    assert!(
        sql.contains("WHEN (SELECT nav_url FROM navigation_context) IS NULL THEN 'unknown'"),
        "missing raw-window navigation context must not classify same/cross origin, got: {sql}",
    );
    assert!(
        sql.contains("WHEN (SELECT navigation_context_status FROM navigation_context) IN ('none', 'ambiguous')"),
        "ambiguous raw-window navigation context must not classify same/cross origin, got: {sql}",
    );
    assert!(
        sql.contains("WHEN (SELECT target_renderer_upids FROM navigation_context) IS NULL"),
        "missing target renderer context must not report other_renderer/browser-only, got: {sql}",
    );
    assert!(
        sql.contains("THEN 'ambiguous_navigation_context'"),
        "renderer relation source must flag ambiguous navigation context, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_summary_sql_compares_full_origin_for_same_origin() {
    let sql =
        chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters::default())
            .expect("resource summary builder must succeed");
    assert!(
        sql.contains("AS url_origin"),
        "summary should expose normalized origin evidence, got: {sql}",
    );
    assert!(
        sql.contains("SUBSTR(rr.url_key, 1, INSTR(rr.url_key, '://') + 2)"),
        "same-origin comparison must include scheme, not host only, got: {sql}",
    );
    assert!(
        sql.contains("INSTR(SUBSTR(rr.url_key, INSTR(rr.url_key, '://') + 3), '?')"),
        "origin/host extraction must stop before query-only URLs, got: {sql}",
    );
    assert!(
        sql.contains("THEN 'same_origin'"),
        "same-origin label should still be available after origin normalization, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_summary_sql_compares_grouped_navigation_key() {
    let sql = chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters {
        url_grouping: Some(ChromePageLoadResourceUrlGrouping::WithoutQuery),
        ..Default::default()
    })
    .expect("resource summary builder must succeed");
    assert!(
            sql.contains("WHEN rr.url_key = CASE WHEN INSTR(COALESCE((SELECT nav_url FROM navigation_context), ''), '?') > 0"),
            "navigation_url classification must apply the same URL grouping to nav_url, got: {sql}",
        );
    assert!(
            !sql.contains("WHEN rr.url_key = COALESCE((SELECT nav_url FROM navigation_context), '')"),
            "navigation_url classification must not compare grouped resource keys to raw nav_url, got: {sql}",
        );
}

#[test]
fn chrome_page_load_resource_pipeline_sql_requires_url_seed() {
    let err =
        chrome_page_load_resource_pipeline_sql(ChromePageLoadResourcePipelineFilters::default())
            .expect_err("pipeline must require a URL seed");
    assert!(
        err.to_string().contains("url_substring") && err.to_string().contains("example_slice_id"),
        "got: {err}"
    );
}

#[test]
fn chrome_page_load_resource_pipeline_sql_builds_url_drilldown() {
    let sql = chrome_page_load_resource_pipeline_sql(ChromePageLoadResourcePipelineFilters {
        window: ChromePageLoadWindowFilters {
            page_load_id: Some(7),
            phase: Some(ChromePageLoadPhase::NavigationToFcp),
            start_ts_ns: Some(1000),
            end_ts_ns: Some(2000),
            ..Default::default()
        },
        url_substring: Some("main.js"),
        example_slice_id: Some(54333),
        url_grouping: Some(ChromePageLoadResourceUrlGrouping::WithoutQuery),
        limit: None,
    })
    .expect("pipeline SQL builder must succeed");
    assert!(
        sql.contains("WITH RECURSIVE"),
        "pipeline must use recursive descendants for script/layout rollup, got: {sql}",
    );
    assert!(
        sql.contains("INSTR(rc.url, 'main.js') > 0"),
        "url_substring must use literal INSTR matching, got: {sql}",
    );
    assert!(
        sql.contains("matched_by") && sql.contains("matched_url_seed"),
        "pipeline must expose URL seed/match evidence, got: {sql}",
    );
    assert!(
        sql.contains("example_url_args")
            && sql.contains("example_min_url_priority")
            && sql.contains("SELECT MIN(eua.example_url) AS example_url"),
        "example_slice_id URL lookup must use prioritized and deterministic URL args, got: {sql}",
    );
    assert!(
        sql.contains("raw_script_selected_urls") && sql.contains("MIN(rsu.url) AS url"),
        "script URL lookup must collapse same-priority URL ties deterministically, got: {sql}",
    );
    assert!(
        sql.contains("s.id = 54333"),
        "example_slice_id must seed URL lookup, got: {sql}",
    );
    assert!(
        sql.contains("request_span_ms"),
        "pipeline must expose request span evidence, got: {sql}",
    );
    assert!(
        sql.contains("background_parse_ms"),
        "pipeline must expose background parse evidence, got: {sql}",
    );
    assert!(
        sql.contains("max_evaluate_ms"),
        "pipeline must expose script evaluate evidence, got: {sql}",
    );
    assert!(
        sql.contains("evidence_boundary"),
        "pipeline must carry attribution boundary text, got: {sql}",
    );
    assert!(
        sql.contains("LIMIT 30"),
        "default pipeline limit should be compact, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_pipeline_sql_clips_script_metrics_to_window() {
    let sql = chrome_page_load_resource_pipeline_sql(ChromePageLoadResourcePipelineFilters {
        window: ChromePageLoadWindowFilters {
            page_load_id: Some(7),
            phase: Some(ChromePageLoadPhase::NavigationToFcp),
            start_ts_ns: Some(1000),
            end_ts_ns: Some(2000),
            ..Default::default()
        },
        url_substring: Some("main.js"),
        ..Default::default()
    })
    .expect("pipeline SQL builder must succeed");
    assert!(
        sql.contains("MAX(s.ts, MAX(rw.start_ts, 1000)) AS overlap_start_ts"),
        "script slices must compute clipped overlap starts, got: {sql}",
    );
    assert!(
        sql.contains("MIN(s.ts + s.dur, MIN(rw.end_ts, 2000)) AS overlap_end_ts"),
        "script slices must compute clipped overlap ends, got: {sql}",
    );
    assert!(
        sql.contains("THEN ss.overlap_dur ELSE 0 END) / 1e6, 3)"),
        "script totals must aggregate clipped overlap durations, got: {sql}",
    );
    assert!(
        sql.contains("ROUND(MAX(ss.overlap_thread_dur) / 1e6, 3) AS max_script_cpu_ms"),
        "script CPU must be prorated to the clipped overlap, got: {sql}",
    );
    assert!(
        sql.contains("ORDER BY s2.overlap_dur DESC, s2.dur DESC"),
        "example script id should rank by clipped overlap before full duration, got: {sql}",
    );
    assert!(
        !sql.contains("THEN ss.dur ELSE 0 END) / 1e6, 3)"),
        "windowed script totals must not use full slice duration, got: {sql}",
    );
    let compact = compact_sql(&sql);
    assert!(
        compact.contains(
            "child.name GLOB '*Style*' OR child.name = 'Blink.Style.UpdateTime' ) AND NOT ( child.name GLOB '*ForcedStyle*'"
        ),
        "pipeline style bucket must exclude forced-style/layout slices, got: {sql}",
    );
    assert!(
        compact.contains(
            "child.name GLOB '*Layout*' OR child.name = 'Blink.Layout.UpdateTime' OR child.name = 'Layout' ) AND NOT ( child.name GLOB '*ForcedStyle*'"
        ) && compact.contains(
            "AND NOT ( child.name GLOB '*Style*' OR child.name = 'Blink.Style.UpdateTime' ) THEN CASE"
        ),
        "pipeline layout bucket must exclude forced and style buckets, got: {sql}",
    );
}

#[test]
fn chrome_page_load_resource_timing_evidence_sql_probes_phase_and_incomplete_signals() {
    let sql = chrome_page_load_resource_timing_evidence_sql(ChromePageLoadWindowFilters {
        page_load_id: Some(7),
        phase: Some(ChromePageLoadPhase::FcpToLoad),
        start_ts_ns: Some(1000),
        end_ts_ns: Some(2000),
        ..Default::default()
    })
    .expect("resource timing evidence SQL builder must succeed");

    assert!(
        sql.contains("resource_timing_probe AS"),
        "probe SQL must expose a named CTE, got: {sql}",
    );
    assert!(
        sql.contains("lower(s.name) GLOB '*dns*'"),
        "probe must look for network phase-like slice names, got: {sql}",
    );
    assert!(
        sql.contains("network_phase_arg_count"),
        "probe must count phase-like arg keys, got: {sql}",
    );
    assert!(
        sql.contains("incomplete_resource_slice_count"),
        "probe must count incomplete URL-bearing resource slices, got: {sql}",
    );
    assert!(
        sql.contains("MAX(rw.start_ts, 1000)"),
        "raw start bound must AND with page-load phase window, got: {sql}",
    );
    assert!(
        sql.contains("MIN(rw.end_ts, 2000)"),
        "raw end bound must AND with page-load phase window, got: {sql}",
    );
    assert!(
        sql.contains("phase_breakdown_available"),
        "probe must return the machine-readable capability flag, got: {sql}",
    );
}

#[test]
fn chrome_page_load_script_hotspots_sql_defaults_to_grouped_script_scan() {
    let sql = chrome_page_load_script_hotspots_sql(ChromePageLoadScriptHotspotsFilters::default())
        .expect("script builder must succeed");
    assert!(
        sql.contains("WITH RECURSIVE script_slices AS"),
        "script SQL must use recursive CTEs for descendant rollups, got: {sql}",
    );
    assert!(
        !sql.contains("chrome.page_loads"),
        "no-window script SQL must not include page-loads, got: {sql}",
    );
    assert!(
        sql.contains("AND (t.is_main_thread = 1 OR t.name GLOB 'Cr*Main')"),
        "script SQL must scope to renderer main-thread-style work, got: {sql}",
    );
    assert!(
        sql.contains("'EvaluateScript', 'v8.run', 'FunctionCall'"),
        "script SQL must match canonical JS execution slices, got: {sql}",
    );
    assert!(
        sql.contains("descendant_rollup AS"),
        "script SQL must aggregate descendant style/layout work, got: {sql}",
    );
    for column in [
        "forced_style_layout_ms",
        "style_recalc_ms",
        "layout_ms",
        "example_slice_id",
    ] {
        assert!(
            sql.contains(column),
            "script SQL must expose {column}, got: {sql}",
        );
    }
    assert!(
        sql.contains("AS overlap_dur"),
        "script SQL must compute per-slice window overlap duration, got: {sql}",
    );
    assert!(
        sql.contains("ROUND(SUM(ss.overlap_dur) / 1e6, 3) AS total_wall_ms"),
        "script total wall time must aggregate clipped overlap duration, got: {sql}",
    );
    assert!(
        sql.contains("ROUND(MAX(ss.overlap_dur) / 1e6, 3) AS max_wall_ms"),
        "script max wall time must use clipped overlap duration, got: {sql}",
    );
    assert!(
        sql.contains("* 1.0 / s.dur"),
        "clipped CPU estimate must avoid integer division, got: {sql}",
    );
    assert!(
        sql.contains("ORDER BY s2.overlap_dur DESC, s2.dur DESC, s2.id ASC"),
        "example_slice_id must rank by clipped overlap duration, got: {sql}",
    );
    assert!(
        sql.contains("JOIN script_slices root ON root.id = sd.root_id"),
        "descendant rollups must see the root clipped window, got: {sql}",
    );
    assert!(
        sql.contains("root.overlap_start_ts"),
        "descendant style/layout duration must be clipped to root overlap start, got: {sql}",
    );
    assert!(
        sql.contains("root.overlap_end_ts"),
        "descendant style/layout duration must be clipped to root overlap end, got: {sql}",
    );
    let compact = compact_sql(&sql);
    assert!(
        compact.contains(
            "d.name GLOB '*Recalculate*Style*' OR d.name GLOB '*UpdateStyle*' OR d.name GLOB '*StyleRecalc*' ) AND NOT ( d.name GLOB '*Forced*Layout*'"
        ),
        "script style bucket must exclude forced-style/layout slices, got: {sql}",
    );
    assert!(
        compact.contains(
            "d.name GLOB '*Layout*' OR d.name GLOB '*UpdateLayout*' ) AND NOT ( d.name GLOB '*Forced*Layout*'"
        ) && compact.contains(
            "AND NOT ( d.name GLOB '*Recalculate*Style*' OR d.name GLOB '*UpdateStyle*' OR d.name GLOB '*StyleRecalc*' ) THEN CASE"
        ),
        "script layout bucket must exclude forced and style buckets, got: {sql}",
    );
    assert!(
        sql.contains("HAVING SUM(ss.overlap_dur) >= 20000000"),
        "default grouped script threshold must be 20 ms, got: {sql}",
    );
    assert!(
        sql.contains("ORDER BY total_wall_ms DESC, max_wall_ms DESC, first_start_ms ASC"),
        "script SQL must order by aggregate wall time, got: {sql}",
    );
}

#[test]
fn chrome_page_load_script_hotspots_sql_with_window_and_process_filters() {
    let sql = chrome_page_load_script_hotspots_sql(ChromePageLoadScriptHotspotsFilters {
        process_name: Some("Renderer"),
        upid: Some(14),
        window: ChromePageLoadWindowFilters {
            navigation_id: Some(7),
            phase: Some(ChromePageLoadPhase::NavigationToFcp),
            start_ts_ns: Some(1000),
            end_ts_ns: Some(2000),
            ..Default::default()
        },
        min_total_ms: Some(0.0),
        limit: Some(25),
        ..Default::default()
    })
    .expect("script window builder must succeed");
    assert!(
        sql.contains("INCLUDE PERFETTO MODULE chrome.page_loads; WITH RECURSIVE"),
        "windowed script SQL must include page-loads before recursive CTEs, got: {sql}",
    );
    assert!(
        sql.contains("WHERE navigation_id = 7 "),
        "navigation_id must match only chrome_page_loads.navigation_id, got: {sql}",
    );
    assert!(
        !sql.contains("WHERE id = 7 "),
        "navigation_id must not also match chrome_page_loads.id, got: {sql}",
    );
    assert!(
        sql.contains("s.ts + s.dur > MAX(sw.start_ts, 1000)"),
        "script overlap lower predicate must use effective start, got: {sql}",
    );
    assert!(
        sql.contains("s.ts < MIN(sw.end_ts, 2000)"),
        "script overlap upper predicate must use effective end, got: {sql}",
    );
    assert!(
        sql.contains("AND MIN(sw.end_ts, 2000) > MAX(sw.start_ts, 1000)"),
        "script SQL must reject empty/reversed effective windows, got: {sql}",
    );
    assert!(
        sql.contains("MAX(s.ts, MAX(sw.start_ts, 1000)) AS overlap_start_ts"),
        "script SQL must clip slice starts to effective window start, got: {sql}",
    );
    assert!(
        sql.contains("MIN(s.ts + s.dur, MIN(sw.end_ts, 2000)) AS overlap_end_ts"),
        "script SQL must clip slice ends to effective window end, got: {sql}",
    );
    assert!(
        sql.contains("AND p.name = 'Renderer'"),
        "process_name filter must be emitted, got: {sql}",
    );
    assert!(
        sql.contains("AND p.upid = 14"),
        "upid filter must be emitted, got: {sql}",
    );
    assert!(
        sql.contains("HAVING SUM(ss.overlap_dur) >= 0"),
        "min_total_ms=0 must use clipped overlap duration, got: {sql}",
    );
    assert!(
        sql.contains("LIMIT 25"),
        "limit must be honored, got: {sql}"
    );
}

#[test]
fn chrome_page_load_script_hotspots_sql_validates_inputs() {
    let err = chrome_page_load_script_hotspots_sql(ChromePageLoadScriptHotspotsFilters {
        window: ChromePageLoadWindowFilters {
            page_load_id: Some(1),
            navigation_id: Some(7),
            ..Default::default()
        },
        ..Default::default()
    })
    .expect_err("page_load_id plus navigation_id must error");
    assert!(err.to_string().contains("mutually exclusive"), "got: {err}");

    let err = chrome_page_load_script_hotspots_sql(ChromePageLoadScriptHotspotsFilters {
        min_total_ms: Some(f64::NAN),
        ..Default::default()
    })
    .expect_err("NaN min_total_ms must error");
    assert!(err.to_string().contains("min_total_ms"), "got: {err}");
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
