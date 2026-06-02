use super::*;

#[test]
fn sanitize_allows_valid_patterns() {
    assert_eq!(sanitize_glob_param("chrome_*").unwrap(), "chrome_*");
    assert_eq!(sanitize_glob_param("process").unwrap(), "process");
    assert_eq!(sanitize_glob_param("a.b-c:d").unwrap(), "a.b-c:d");
}

#[test]
fn sanitize_rejects_injection() {
    assert!(sanitize_glob_param("'; DROP TABLE --").is_err());
    assert!(sanitize_glob_param("a b").is_err());
}

#[test]
fn sql_string_literal_allows_common_process_names() {
    assert_eq!(
        sql_string_literal("com.android.chrome").unwrap(),
        "'com.android.chrome'"
    );
    assert_eq!(
        sql_string_literal("/system/bin/init").unwrap(),
        "'/system/bin/init'"
    );
    assert_eq!(sql_string_literal("GPU Process").unwrap(), "'GPU Process'");
    assert_eq!(
        sql_string_literal("Browser (123)").unwrap(),
        "'Browser (123)'"
    );
}

#[test]
fn sql_string_literal_doubles_single_quotes() {
    assert_eq!(sql_string_literal("it's").unwrap(), "'it''s'");
}

#[test]
fn sql_string_literal_rejects_control_characters() {
    assert!(sql_string_literal("bad\x00name").is_err());
    assert!(sql_string_literal("bad\nnewline").is_err());
}

#[test]
fn chrome_resource_url_origin_expr_keeps_scheme_port_and_strips_path_query_fragment() {
    let origin = chrome_resource_url_origin_expr("u");
    assert!(
        origin.contains("SUBSTR(u, 1, INSTR(u, '://') + 2)"),
        "origin expression must include scheme, got: {origin}",
    );
    assert!(
        origin.contains("INSTR(SUBSTR(u, INSTR(u, '://') + 3), '?')"),
        "origin expression must treat query as an authority terminator, got: {origin}",
    );
    assert!(
        origin.contains("INSTR(SUBSTR(u, INSTR(u, '://') + 3), '#')"),
        "origin expression must treat fragment as an authority terminator, got: {origin}",
    );
    assert!(
        origin.contains("LOWER("),
        "origin expression should normalize scheme/host case, got: {origin}",
    );
}

#[test]
fn chrome_url_arg_priority_prefers_real_frame_url_over_placeholder_context_urls() {
    let priority = chrome_url_arg_priority_expr("a");
    let placeholder = priority
        .find("LOWER(a.display_value) IN ('http://unisolated.invalid/'")
        .expect("placeholder URL demotion must be explicit");
    let current_frame = priority
        .find("LOWER(a.flat_key) GLOB '*current_frame_host.url'")
        .expect("current-frame URL must be recognized before generic URL fallback");
    let process_lock = priority
        .find("LOWER(a.flat_key) GLOB '*process_lock_url'")
        .expect("process-lock URL must be demoted before generic URL fallback");
    let request_url = priority
        .find("LOWER(a.flat_key) GLOB '*request*url*'")
        .expect("request URL fallback must remain available");
    let generic_url = priority
        .find("LOWER(a.flat_key) LIKE '%url%'")
        .expect("generic URL fallback must remain available");

    assert!(
        placeholder < current_frame,
        "placeholder URL demotion should run before real URL cases, got: {priority}",
    );
    assert!(
        current_frame < process_lock,
        "current-frame URLs must beat process_lock/site URL fields, got: {priority}",
    );
    assert!(
        process_lock < request_url,
        "process_lock/site URL fields must not be caught by request-url fallback, got: {priority}",
    );
    assert!(
        request_url < generic_url,
        "request/script URL fallback should still beat generic URL fallback, got: {priority}",
    );
    assert!(
            priority.contains("THEN 90") && priority.contains("THEN 8"),
            "placeholder/context URL priorities should remain worse than real URL priorities, got: {priority}",
        );
}

#[test]
fn slice_descendants_breakdown_sql_builds_bounded_recursive_cte() {
    let sql = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
        slice_ids: &[10, 11],
        min_dur_ms: Some(0.5),
        max_depth: Some(4),
        include_args: true,
        row_limit: 25,
    })
    .expect("builder must succeed");

    assert!(sql.contains("roots(root_id) AS (VALUES (10), (11))"));
    assert!(sql.contains("JOIN slice child ON child.parent_id = d.slice_id"));
    assert!(sql.contains("WHERE d.depth < 4"));
    assert!(sql.contains("WHERE d.depth > 0"));
    assert!(sql.contains("AND s.dur >= 500000"));
    assert!(sql.contains("AS example_args"));
    assert!(sql.contains("LIMIT 25"));
    assert!(
        !sql.contains("WHERE depth"),
        "recursive CTE must qualify depth to avoid ambiguous-column errors: {sql}",
    );
}

#[test]
fn slice_descendants_breakdown_sql_picks_longest_dur_example_and_renames_first_ts() {
    let sql = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
        slice_ids: &[1],
        min_dur_ms: None,
        max_depth: None,
        include_args: false,
        row_limit: 100,
    })
    .expect("builder must succeed");

    assert!(
            sql.contains("ROW_NUMBER() OVER ( PARTITION BY d.root_id, d.depth, s.name ORDER BY s.dur DESC, s.id ASC )"),
            "example_slice_id must come from longest-duration descendant: {sql}",
        );
    assert!(
        sql.contains("MAX(CASE WHEN rn = 1 THEN slice_id END) AS example_slice_id"),
        "longest-dur slice id must be surfaced via rn=1 selector: {sql}",
    );
    assert!(
        sql.contains("MIN(ts) AS first_ts_ns"),
        "first_ts must be renamed to first_ts_ns to disambiguate units (ns vs ms): {sql}",
    );
    assert!(
        !sql.contains(" AS first_ts ") && !sql.contains(" AS first_ts,"),
        "no bare first_ts column should remain after rename: {sql}",
    );
    assert!(
        !sql.contains("MIN(s.id)"),
        "old MIN(s.id) example selector must be removed: {sql}",
    );
}

#[test]
fn slice_descendants_breakdown_sql_deduplicates_roots_preserving_order() {
    let sql = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
        slice_ids: &[10, 11, 10, 12, 11],
        min_dur_ms: None,
        max_depth: None,
        include_args: false,
        row_limit: 100,
    })
    .expect("builder must succeed");

    assert!(
        sql.contains("roots(root_id) AS (VALUES (10), (11), (12))"),
        "duplicate roots must be removed before recursive expansion: {sql}",
    );
    assert!(
        !sql.contains("(10), (11), (10)"),
        "duplicate roots would inflate descendant aggregates: {sql}",
    );
}

#[test]
fn slice_descendants_breakdown_sql_rejects_unbounded_inputs() {
    let err = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
        slice_ids: &[],
        min_dur_ms: None,
        max_depth: None,
        include_args: false,
        row_limit: 100,
    })
    .expect_err("empty roots must reject");
    assert!(err.to_string().contains("slice_ids"));

    let err = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
        slice_ids: &[1],
        min_dur_ms: None,
        max_depth: Some(0),
        include_args: false,
        row_limit: 100,
    })
    .expect_err("zero depth must reject");
    assert!(err.to_string().contains("max_depth"));

    let err = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
        slice_ids: &[1],
        min_dur_ms: Some(f64::INFINITY),
        max_depth: None,
        include_args: false,
        row_limit: 100,
    })
    .expect_err("non-finite duration must reject");
    assert!(err.to_string().contains("min_dur_ms"));

    let err = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
        slice_ids: &[1],
        min_dur_ms: None,
        max_depth: None,
        include_args: false,
        row_limit: 0,
    })
    .expect_err("zero row_limit must reject");
    assert!(err.to_string().contains("row_limit"));
}

#[test]
fn slice_descendants_breakdown_sql_validates_values_before_size_caps() {
    // Construct a list that is both oversized AND contains a negative id.
    // The value-shape error must surface first so callers get an
    // actionable message rather than a misleading size complaint.
    let mut ids = vec![-7_i64];
    ids.extend(0_i64..=MAX_SLICE_DESCENDANTS_ROOTS as i64);
    let err = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
        slice_ids: &ids,
        min_dur_ms: None,
        max_depth: None,
        include_args: false,
        row_limit: 100,
    })
    .expect_err("must reject when a negative id is present");
    assert!(
        err.to_string().contains("non-negative"),
        "negative-id error must surface before root-count cap, got: {err}",
    );
}
