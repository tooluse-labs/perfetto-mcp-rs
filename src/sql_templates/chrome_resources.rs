use crate::error::PerfettoError;
use crate::params::{
    ChromePageLoadResourceHotspotsFilters, ChromePageLoadResourcePipelineFilters,
    ChromePageLoadResourceSummaryFilters, ChromePageLoadResourceUrlGrouping,
    ChromePageLoadWindowFilters,
};

use super::chrome_common::{
    append_chrome_page_load_window_cte, append_chrome_page_load_window_cte_body,
    chrome_tool_row_limit, chrome_tool_row_limit_with_default, duration_ms_to_ns,
    validate_chrome_page_load_window, ChromePageLoadWindowSql,
};
use super::sanitize::sql_string_literal;
#[derive(Debug)]
struct ChromeResourceWindowExprs {
    start_bound: Option<String>,
    end_bound: Option<String>,
    anchor_expr: String,
    overlap_start_expr: String,
    overlap_end_expr: String,
    window_dur_expr: Option<String>,
}

fn chrome_resource_window_exprs(page_window: ChromePageLoadWindowSql) -> ChromeResourceWindowExprs {
    let start_bound = match (page_window.phase.is_some(), page_window.start_ts_ns) {
        (true, Some(ts)) => Some(format!("MAX(rw.start_ts, {ts})")),
        (true, None) => Some("rw.start_ts".to_owned()),
        (false, Some(ts)) => Some(ts.to_string()),
        (false, None) => None,
    };
    let end_bound = match (page_window.phase.is_some(), page_window.end_ts_ns) {
        (true, Some(ts)) => Some(format!("MIN(rw.end_ts, {ts})")),
        (true, None) => Some("rw.end_ts".to_owned()),
        (false, Some(ts)) => Some(ts.to_string()),
        (false, None) => None,
    };
    let anchor_expr = if page_window.phase.is_some() {
        "rw.navigation_start_ts".to_owned()
    } else if let Some(start) = page_window.start_ts_ns {
        start.to_string()
    } else {
        "trace_start()".to_owned()
    };
    let overlap_start_expr = match &start_bound {
        Some(bound) => format!("MAX(s.ts, {bound})"),
        None => "s.ts".to_owned(),
    };
    let overlap_end_expr = match &end_bound {
        Some(bound) => {
            format!("CASE WHEN s.dur >= 0 THEN MIN(s.ts + s.dur, {bound}) ELSE {bound} END")
        }
        None => "CASE WHEN s.dur >= 0 THEN s.ts + s.dur ELSE trace_end() END".to_owned(),
    };
    let window_dur_expr = match (&start_bound, &end_bound) {
        (Some(start), Some(end)) => Some(format!("({end} - {start})")),
        _ => None,
    };

    ChromeResourceWindowExprs {
        start_bound,
        end_bound,
        anchor_expr,
        overlap_start_expr,
        overlap_end_expr,
        window_dur_expr,
    }
}

fn append_chrome_resource_candidates_cte(
    sql: &mut String,
    page_window: ChromePageLoadWindowSql,
    exprs: &ChromeResourceWindowExprs,
    min_dur_ns: i64,
) {
    let pct_expr = match &exprs.window_dur_expr {
        Some(expr) => format!(
            "CASE WHEN {expr} > 0 \
                  THEN ROUND(({} - {}) * 100.0 / {expr}, 2) \
             END AS pct_of_window",
            exprs.overlap_end_expr, exprs.overlap_start_expr
        ),
        None => "NULL AS pct_of_window".to_owned(),
    };
    let window_dur_expr = match &exprs.window_dur_expr {
        Some(expr) => format!("CASE WHEN {expr} > 0 THEN {expr} END AS window_dur_ns"),
        None => "NULL AS window_dur_ns".to_owned(),
    };

    let url_arg_priority_expr = chrome_url_arg_priority_expr("a");

    sql.push_str(&format!(
        "resource_candidate_slices AS ( \
           SELECT \
             s.id, \
             s.ts, \
             s.dur, \
             s.arg_set_id, \
             ROUND((s.ts - {}) / 1e6, 3) AS start_ms, \
             CASE WHEN s.dur >= 0 \
                  THEN ROUND((s.ts + s.dur - {}) / 1e6, 3) \
             END AS end_ms, \
             CASE WHEN s.dur >= 0 THEN ROUND(s.dur / 1e6, 3) END AS dur_ms, \
             CASE WHEN s.dur < 0 THEN 'incomplete_duration' ELSE 'complete' END \
               AS slice_duration_status, \
             {} AS overlap_start_ts, \
             {} AS overlap_end_ts, \
             ({} - {}) AS overlap_dur, \
             ROUND(({} - {}) / 1e6, 3) AS overlap_ms, \
             ROUND(({} - {}) / 1e6, 3) AS overlap_start_ms, \
             ROUND(({} - {}) / 1e6, 3) AS overlap_end_ms, \
             {pct_expr}, \
             {window_dur_expr}, \
             s.name, \
              COALESCE(p_thread.name, p_process.name, p_parent_thread.name, p_parent_process.name) \
                AS process_name, \
              COALESCE(p_thread.upid, p_process.upid, p_parent_thread.upid, p_parent_process.upid) \
                AS upid, \
              COALESCE(p_thread.pid, p_process.pid, p_parent_thread.pid, p_parent_process.pid) \
                AS pid, \
              COALESCE(t.name, parent_t.name) AS thread_name, \
             COALESCE( \
               MAX(CASE WHEN a.flat_key IN ('debug.priority', 'priority') \
                    THEN a.display_value END), \
               MAX(CASE WHEN a.key = 'priority' THEN a.display_value END) \
             ) AS priority \
           FROM slice s \
           LEFT JOIN track tr ON s.track_id = tr.id \
           LEFT JOIN thread_track tt ON s.track_id = tt.id \
           LEFT JOIN thread t ON tt.utid = t.utid \
           LEFT JOIN process p_thread ON t.upid = p_thread.upid \
           LEFT JOIN process_track pt ON s.track_id = pt.id \
           LEFT JOIN process p_process ON pt.upid = p_process.upid \
           LEFT JOIN thread_track parent_tt ON tr.parent_id = parent_tt.id \
           LEFT JOIN thread parent_t ON parent_tt.utid = parent_t.utid \
           LEFT JOIN process p_parent_thread ON parent_t.upid = p_parent_thread.upid \
           LEFT JOIN process_track parent_pt ON tr.parent_id = parent_pt.id \
           LEFT JOIN process p_parent_process ON parent_pt.upid = p_parent_process.upid \
           LEFT JOIN args a ON s.arg_set_id = a.arg_set_id ",
        exprs.anchor_expr,
        exprs.anchor_expr,
        exprs.overlap_start_expr,
        exprs.overlap_end_expr,
        exprs.overlap_end_expr,
        exprs.overlap_start_expr,
        exprs.overlap_end_expr,
        exprs.overlap_start_expr,
        exprs.overlap_start_expr,
        exprs.anchor_expr,
        exprs.overlap_end_expr,
        exprs.anchor_expr,
    ));
    if page_window.phase.is_some() {
        sql.push_str("CROSS JOIN resource_window rw ");
    }
    sql.push_str(&format!(
        "WHERE ({} - {}) >= {min_dur_ns} \
           AND ( \
             s.name GLOB '*Resource*' OR \
              s.name GLOB '*URLLoader*' OR \
              s.name GLOB '*URLRequest*' OR \
              s.name GLOB '*Network*' OR \
              s.name GLOB '*Request*' OR \
              s.name GLOB '*Fetch*' OR \
              s.name GLOB '*XHR*' \
            ) \
           AND NOT ( \
             s.name GLOB '*PageLoadMetrics*' OR \
             s.name GLOB '*DidCommitProvisionalLoad*' OR \
             s.name GLOB '*DidStartProvisionalLoad*' OR \
             s.name GLOB '*DidStopLoading*' OR \
             s.name GLOB '*DidFinishLoad*' \
           )",
        exprs.overlap_end_expr, exprs.overlap_start_expr
    ));
    if let Some(bound) = &exprs.start_bound {
        sql.push_str(&format!(" AND {} > {bound}", exprs.overlap_end_expr));
    }
    if let Some(bound) = &exprs.end_bound {
        sql.push_str(&format!(" AND s.ts < {bound}"));
    }
    if page_window.phase.is_some() {
        sql.push_str(" AND rw.start_ts IS NOT NULL AND rw.end_ts IS NOT NULL");
    }
    if let (Some(start), Some(end)) = (&exprs.start_bound, &exprs.end_bound) {
        sql.push_str(&format!(" AND {end} > {start}"));
    }
    sql.push_str(
        " GROUP BY \
             s.id, s.ts, s.dur, s.arg_set_id, s.name, \
             p_thread.name, p_process.name, p_parent_thread.name, p_parent_process.name, \
             p_thread.upid, p_process.upid, p_parent_thread.upid, p_parent_process.upid, \
             p_thread.pid, p_process.pid, p_parent_thread.pid, p_parent_process.pid, \
             t.name, parent_t.name \
         ), \
         resource_candidate_urls AS ( \
           SELECT \
             rcs.id, \
             a.display_value AS url, \
             ",
    );
    sql.push_str(&url_arg_priority_expr);
    sql.push_str(
        " AS url_priority \
           FROM resource_candidate_slices rcs \
           JOIN args a ON rcs.arg_set_id = a.arg_set_id \
           WHERE a.display_value IS NOT NULL \
             AND a.display_value != '' \
         ), \
         resource_candidate_min_url_priority AS ( \
           SELECT id, MIN(url_priority) AS url_priority \
           FROM resource_candidate_urls \
           WHERE url_priority < 99 \
           GROUP BY id \
         ), \
         resource_candidate_selected_urls AS ( \
           SELECT rcu.id, MIN(rcu.url) AS url \
           FROM resource_candidate_urls rcu \
           JOIN resource_candidate_min_url_priority rcup \
             ON rcup.id = rcu.id AND rcup.url_priority = rcu.url_priority \
           GROUP BY rcu.id \
         ), \
         resource_candidates AS ( \
           SELECT DISTINCT \
             rcs.id, \
             rcs.ts, \
             rcs.dur, \
             rcs.start_ms, \
             rcs.end_ms, \
             rcs.dur_ms, \
             rcs.slice_duration_status, \
             rcs.overlap_start_ts, \
             rcs.overlap_end_ts, \
             rcs.overlap_dur, \
             rcs.overlap_ms, \
             rcs.overlap_start_ms, \
             rcs.overlap_end_ms, \
             rcs.pct_of_window, \
             rcs.window_dur_ns, \
             rcs.name, \
             rcs.process_name, \
             rcs.upid, \
             rcs.pid, \
             rcs.thread_name, \
             rcsu.url, \
             rcs.priority \
           FROM resource_candidate_slices rcs \
           JOIN resource_candidate_selected_urls rcsu ON rcsu.id = rcs.id \
         ) ",
    );
}

/// SQL builder for `chrome_page_load_resource_hotspots`.
///
/// This is intentionally slice-based rather than a HAR clone: Chrome traces do
/// not always expose a stable resource timing stdlib view, but resource-shaped
/// slices with URL args are enough to surface page-load blockers like long
/// NetworkService `GetResource`/`URLLoader` work. Window overlap is used so a
/// resource that starts before a phase and finishes inside it is still counted.
pub fn chrome_page_load_resource_hotspots_sql(
    filters: ChromePageLoadResourceHotspotsFilters,
) -> Result<String, PerfettoError> {
    let ChromePageLoadResourceHotspotsFilters {
        window,
        min_dur_ms,
        limit,
    } = filters;
    let page_window = validate_chrome_page_load_window(window)?;
    let min_dur_ns = duration_ms_to_ns("min_dur_ms", min_dur_ms, 50_000_000)?;
    let row_limit = chrome_tool_row_limit(limit)?;

    let mut sql = String::new();
    append_chrome_page_load_window_cte(&mut sql, "resource_window", window, page_window);
    if sql.is_empty() {
        sql.push_str("WITH ");
    } else {
        sql.push_str(", ");
    }

    let exprs = chrome_resource_window_exprs(page_window);
    append_chrome_resource_candidates_cte(&mut sql, page_window, &exprs, min_dur_ns);
    sql.push_str(
        " SELECT \
           id, \
           ts, \
           start_ms, \
           end_ms, \
           dur_ms, \
           slice_duration_status, \
           overlap_ms, \
           pct_of_window, \
           name, \
           process_name, \
           upid, \
           pid, \
           thread_name, \
           url \
         FROM resource_candidates ",
    );
    sql.push_str(&format!(
        "ORDER BY overlap_ms DESC, dur_ms DESC, start_ms ASC LIMIT {row_limit}"
    ));
    Ok(sql)
}

/// SQL builder for `chrome_page_load_resource_summary`.
///
/// Aggregates resource/request slices by URL so LLM agents can see page-load
/// blockers at the same level they appear in recommendations (`main.js` /
/// `vendor.js`) without double-reading Renderer and NetworkService slices as
/// unrelated rows. `max_overlap_ms` is the primary ranking metric; summed
/// overlap is also returned but may include nested/parallel trace-slice double
/// counting for the same URL.
pub fn chrome_page_load_resource_summary_sql(
    filters: ChromePageLoadResourceSummaryFilters,
) -> Result<String, PerfettoError> {
    let ChromePageLoadResourceSummaryFilters {
        window,
        min_overlap_ms,
        url_grouping,
        limit,
    } = filters;
    let page_window = validate_chrome_page_load_window(window)?;
    let min_overlap_ns = duration_ms_to_ns("min_overlap_ms", min_overlap_ms, 50_000_000)?;
    let row_limit = chrome_tool_row_limit_with_default(limit, 25)?;

    let mut sql = String::new();
    if page_window.phase.is_some() {
        append_chrome_page_load_window_cte(&mut sql, "resource_window", window, page_window);
        sql.push_str(", ");
    } else {
        sql.push_str("INCLUDE PERFETTO MODULE chrome.page_loads; WITH ");
    }

    let exprs = chrome_resource_window_exprs(page_window);
    append_chrome_resource_candidates_cte(&mut sql, page_window, &exprs, 0);

    let url_key_expr = chrome_resource_url_key_expr("url", url_grouping);
    let nav_url_expr = if page_window.phase.is_some() {
        "(SELECT url FROM resource_window)".to_owned()
    } else {
        chrome_page_load_raw_window_navigation_url_expr(page_window)
    };
    let navigation_match_count_expr = if page_window.phase.is_some() {
        "1".to_owned()
    } else {
        chrome_page_load_raw_window_navigation_count_expr(page_window)
    };
    let navigation_context_status_expr = if page_window.phase.is_some() {
        "'explicit_page_load_window'".to_owned()
    } else {
        chrome_page_load_raw_window_navigation_status_expr(page_window)
    };
    let url_host_expr = chrome_resource_url_host_expr("rr.url_key");
    let nav_origin_expr =
        chrome_resource_url_origin_expr("COALESCE((SELECT nav_url FROM navigation_context), '')");
    let url_origin_expr = chrome_resource_url_origin_expr("rr.url_key");
    let nav_url_key_expr = chrome_resource_url_key_expr(
        "COALESCE((SELECT nav_url FROM navigation_context), '')",
        url_grouping,
    );

    sql.push_str(&format!(
        ", navigation_context AS ( \
           SELECT \
             {nav_url_expr} AS nav_url, \
             {navigation_match_count_expr} AS navigation_match_count, \
             {navigation_context_status_expr} AS navigation_context_status, \
             GROUP_CONCAT(DISTINCT CASE \
               WHEN process_name = 'Renderer' THEN upid \
             END) AS target_renderer_upids \
           FROM resource_candidates \
           WHERE url = {nav_url_expr} \
         ), \
         resource_rows AS ( \
            SELECT resource_candidates.*, {url_key_expr} AS url_key \
            FROM resource_candidates \
          ) \
          SELECT \
            rr.url_key, \
            (SELECT r2.url FROM resource_rows r2 \
             WHERE r2.url_key = rr.url_key \
             ORDER BY r2.overlap_dur DESC, r2.dur DESC, r2.id ASC LIMIT 1) AS example_url, \
            {url_host_expr} AS url_host, \
            {url_origin_expr} AS url_origin, \
            (SELECT nav_url FROM navigation_context) AS navigation_url, \
            (SELECT navigation_context_status FROM navigation_context) AS navigation_context_status, \
            (SELECT navigation_match_count FROM navigation_context) AS navigation_match_count, \
            CASE \
              WHEN rr.url_key GLOB 'chrome://*' OR rr.url_key GLOB 'chrome-extension://*' \
                THEN 'browser_ui_or_extension' \
              WHEN (SELECT navigation_context_status FROM navigation_context) IN ('none', 'ambiguous') \
                THEN 'unknown' \
              WHEN (SELECT nav_url FROM navigation_context) IS NULL THEN 'unknown' \
              WHEN rr.url_key = {nav_url_key_expr} \
                THEN 'navigation_url' \
              WHEN {url_origin_expr} != '' AND {url_origin_expr} = {nav_origin_expr} \
                THEN 'same_origin' \
              WHEN {url_origin_expr} != '' THEN 'cross_origin' \
              ELSE 'unknown' \
            END AS relation_to_navigation, \
            CASE \
              WHEN (SELECT navigation_context_status FROM navigation_context) IN ('none', 'ambiguous') \
                THEN 'unknown' \
              WHEN (SELECT target_renderer_upids FROM navigation_context) IS NULL \
                THEN 'unknown' \
              WHEN MAX(CASE WHEN INSTR( \
                  ',' || (SELECT target_renderer_upids FROM navigation_context) || ',', \
                  ',' || rr.upid || ',' \
                ) > 0 THEN 1 ELSE 0 END) = 1 \
                THEN 'target_renderer' \
              WHEN MAX(CASE WHEN rr.process_name = 'Renderer' THEN 1 ELSE 0 END) = 1 \
                THEN 'other_renderer' \
              ELSE 'browser_or_service_only' \
            END AS renderer_relation, \
            CASE \
              WHEN (SELECT navigation_context_status FROM navigation_context) IN ('none', 'ambiguous') \
                THEN 'low' \
              WHEN (SELECT target_renderer_upids FROM navigation_context) IS NULL \
                THEN 'low' \
              WHEN MAX(CASE WHEN INSTR( \
                  ',' || (SELECT target_renderer_upids FROM navigation_context) || ',', \
                  ',' || rr.upid || ',' \
                ) > 0 THEN 1 ELSE 0 END) = 1 \
                THEN 'medium' \
              WHEN MAX(CASE WHEN rr.process_name = 'Renderer' THEN 1 ELSE 0 END) = 1 \
                THEN 'medium' \
              ELSE 'low' \
            END AS renderer_relation_confidence, \
            CASE \
              WHEN (SELECT navigation_context_status FROM navigation_context) = 'ambiguous' \
                THEN 'ambiguous_navigation_context' \
              WHEN (SELECT navigation_context_status FROM navigation_context) = 'none' \
                THEN 'no_navigation_context' \
              WHEN (SELECT target_renderer_upids FROM navigation_context) IS NULL \
                THEN 'no_renderer_navigation_candidate' \
              WHEN MAX(CASE WHEN INSTR( \
                  ',' || (SELECT target_renderer_upids FROM navigation_context) || ',', \
                  ',' || rr.upid || ',' \
                ) > 0 THEN 1 ELSE 0 END) = 1 \
                THEN 'navigation_url_renderer_candidate_upid_match' \
              WHEN MAX(CASE WHEN rr.process_name = 'Renderer' THEN 1 ELSE 0 END) = 1 \
                THEN 'renderer_process_without_navigation_match' \
              ELSE 'browser_or_service_only' \
            END AS renderer_relation_source, \
            (SELECT target_renderer_upids FROM navigation_context) AS target_renderer_upids, \
            COUNT(*) AS slice_count, \
            SUM(CASE WHEN rr.slice_duration_status = 'incomplete_duration' THEN 1 ELSE 0 END) \
              AS incomplete_duration_slice_count, \
            (SELECT r2.name FROM resource_rows r2 \
             WHERE r2.url_key = rr.url_key \
             ORDER BY r2.overlap_dur DESC, r2.dur DESC, r2.id ASC LIMIT 1) AS primary_slice_name, \
            GROUP_CONCAT(DISTINCT rr.process_name) AS process_names, \
            GROUP_CONCAT(DISTINCT rr.upid) AS upids, \
            GROUP_CONCAT(DISTINCT rr.priority) AS priorities, \
            ROUND(MIN(rr.overlap_start_ms), 3) AS first_start_ms, \
            ROUND(MAX(rr.overlap_end_ms), 3) AS last_end_ms, \
           ROUND((MAX(rr.overlap_end_ts) - MIN(rr.overlap_start_ts)) / 1e6, 3) AS span_ms, \
           ROUND(MAX(rr.overlap_dur) / 1e6, 3) AS max_overlap_ms, \
           ROUND(SUM(rr.overlap_dur) / 1e6, 3) AS summed_overlap_ms, \
           CASE WHEN MAX(rr.window_dur_ns) > 0 \
                THEN ROUND(MAX(rr.overlap_dur) * 100.0 / MAX(rr.window_dur_ns), 2) \
           END AS pct_of_window, \
           (SELECT r2.id FROM resource_rows r2 \
            WHERE r2.url_key = rr.url_key \
            ORDER BY r2.overlap_dur DESC, r2.dur DESC, r2.id ASC LIMIT 1) AS example_slice_id \
         FROM resource_rows rr \
         GROUP BY rr.url_key \
         HAVING MAX(rr.overlap_dur) >= {min_overlap_ns} \
         ORDER BY MAX(rr.overlap_dur) DESC, \
                  (MAX(rr.overlap_end_ts) - MIN(rr.overlap_start_ts)) DESC, \
                  MIN(rr.overlap_start_ts) ASC \
         LIMIT {row_limit}"
    ));
    Ok(sql)
}

/// SQL builder for `chrome_page_load_resource_pipeline`.
///
/// Drills into one URL (by substring or example slice id) and combines the
/// resource lifecycle with parse/evaluate/script-render signals. It remains a
/// trace-slice pipeline, not a HAR reconstruction: request spans are lifecycle
/// evidence, while DNS/TLS/TTFB/cache/download remain hypotheses unless the
/// caller inspects phase-specific slices separately.
pub fn chrome_page_load_resource_pipeline_sql(
    filters: ChromePageLoadResourcePipelineFilters<'_>,
) -> Result<String, PerfettoError> {
    let ChromePageLoadResourcePipelineFilters {
        window,
        url_substring,
        example_slice_id,
        url_grouping,
        limit,
    } = filters;
    if url_substring.is_none() && example_slice_id.is_none() {
        return Err(PerfettoError::InvalidParam(
            "Either `url_substring` or `example_slice_id` must be provided".to_owned(),
        ));
    }
    let page_window = validate_chrome_page_load_window(window)?;
    let row_limit = chrome_tool_row_limit_with_default(limit, 30)?;
    let url_substring_lit = match url_substring {
        Some("") => {
            return Err(PerfettoError::InvalidParam(
                "url_substring must not be empty when set".to_owned(),
            ));
        }
        Some(s) => Some(sql_string_literal(s)?),
        None => None,
    };
    let example_url_filter = match example_slice_id {
        Some(id) => format!("s.id = {id}"),
        None => "0".to_owned(),
    };

    let mut sql = String::new();
    if let Some(phase) = page_window.phase {
        sql.push_str("INCLUDE PERFETTO MODULE chrome.page_loads; WITH RECURSIVE ");
        append_chrome_page_load_window_cte_body(&mut sql, "resource_window", window, phase);
        sql.push_str(", ");
    } else {
        sql.push_str("WITH RECURSIVE ");
    }

    let exprs = chrome_resource_window_exprs(page_window);
    append_chrome_resource_candidates_cte(&mut sql, page_window, &exprs, 0);

    let url_key_expr = chrome_resource_url_key_expr("url", url_grouping);
    let rc_url_key_expr = chrome_resource_url_key_expr("rc.url", url_grouping);
    let eu_example_url_key_expr = chrome_resource_url_key_expr("eu.example_url", url_grouping);
    let ss_url_key_expr = chrome_resource_url_key_expr("ss.url", url_grouping);
    let s2_url_key_expr = chrome_resource_url_key_expr("s2.url", url_grouping);
    let resource_substring_match = match &url_substring_lit {
        Some(lit) => format!("INSTR(rc.url, {lit}) > 0"),
        None => "0".to_owned(),
    };
    let url_substring_seed_expr = url_substring_lit.as_deref().unwrap_or("NULL");
    let example_url_arg_priority_expr = chrome_url_arg_priority_expr("a");
    let example_key_match = format!("{rc_url_key_expr} = {eu_example_url_key_expr}");
    let script_url_arg_priority_expr = chrome_url_arg_priority_expr("a");
    let script_start_bound = exprs.start_bound.as_deref();
    let script_end_bound = exprs.end_bound.as_deref();
    let script_anchor_expr = &exprs.anchor_expr;
    let script_overlap_start_expr = match &exprs.start_bound {
        Some(bound) => format!("MAX(s.ts, {bound})"),
        None => "s.ts".to_owned(),
    };
    let script_overlap_end_expr = match &exprs.end_bound {
        Some(bound) => format!("MIN(s.ts + s.dur, {bound})"),
        None => "s.ts + s.dur".to_owned(),
    };
    let script_overlap_dur_expr =
        format!("({script_overlap_end_expr} - {script_overlap_start_expr})");

    sql.push_str(&format!(
        ", example_url_args AS ( \
           SELECT \
             a.display_value AS example_url, \
             {example_url_arg_priority_expr} AS url_priority \
           FROM slice s \
           JOIN args a ON s.arg_set_id = a.arg_set_id \
           WHERE {example_url_filter} \
             AND a.display_value IS NOT NULL \
             AND a.display_value != '' \
         ), \
         example_min_url_priority AS ( \
           SELECT MIN(url_priority) AS url_priority \
           FROM example_url_args \
           WHERE url_priority < 99 \
         ), \
         example_urls AS ( \
           SELECT MIN(eua.example_url) AS example_url \
           FROM example_url_args eua \
           JOIN example_min_url_priority emup ON emup.url_priority = eua.url_priority \
         ), \
         resource_rows AS ( \
           SELECT \
             rc.*, \
             {rc_url_key_expr} AS url_key, \
             CASE \
               WHEN {resource_substring_match} AND EXISTS ( \
                 SELECT 1 FROM example_urls eu \
                 WHERE eu.example_url IS NOT NULL \
                   AND {example_key_match} \
               ) THEN 'url_substring+example_slice_id' \
               WHEN {resource_substring_match} THEN 'url_substring' \
               ELSE 'example_slice_id' \
             END AS matched_by, \
             CASE \
               WHEN {resource_substring_match} THEN {url_substring_seed_expr} \
               ELSE ( \
                 SELECT eu.example_url FROM example_urls eu \
                 WHERE eu.example_url IS NOT NULL \
                   AND {example_key_match} \
                 ORDER BY eu.example_url LIMIT 1 \
               ) \
             END AS matched_url_seed \
           FROM resource_candidates rc \
           WHERE {resource_substring_match} \
              OR EXISTS ( \
                SELECT 1 FROM example_urls eu \
                WHERE eu.example_url IS NOT NULL \
                  AND {example_key_match} \
              ) \
          ), \
         matched_url_keys AS ( \
           SELECT DISTINCT url_key FROM resource_rows \
         ), \
         raw_script_slice_base AS ( \
            SELECT \
              s.id, \
              s.ts, \
              s.dur, \
              s.arg_set_id, \
              s.thread_dur, \
              {script_overlap_start_expr} AS overlap_start_ts, \
             {script_overlap_end_expr} AS overlap_end_ts, \
             {script_overlap_dur_expr} AS overlap_dur, \
             CASE WHEN s.thread_dur IS NOT NULL AND s.dur > 0 \
                  THEN MAX(MIN( \
                    s.thread_dur * {script_overlap_dur_expr} * 1.0 / s.dur, \
                    {script_overlap_dur_expr} \
                  ), 0.0) \
             END AS overlap_thread_dur, \
             s.name, \
             ROUND(({script_overlap_start_expr} - {script_anchor_expr}) / 1e6, 3) AS start_ms, \
             ROUND(({script_overlap_end_expr} - {script_anchor_expr}) / 1e6, 3) AS end_ms, \
              p.name AS process_name, \
              p.upid AS upid, \
              p.pid AS pid, \
              t.name AS thread_name, \
              MAX(CASE WHEN a.flat_key = 'debug.size' THEN a.int_value END) AS size_bytes \
            FROM slice s \
            LEFT JOIN thread_track tt ON s.track_id = tt.id \
           LEFT JOIN thread t ON tt.utid = t.utid \
           LEFT JOIN process p ON t.upid = p.upid \
           LEFT JOIN args a ON s.arg_set_id = a.arg_set_id ",
    ));
    if page_window.phase.is_some() {
        sql.push_str("CROSS JOIN resource_window rw ");
    }
    sql.push_str(
        "WHERE s.dur > 0 \
           AND ( \
             s.name IN ( \
               'EvaluateScript', 'v8.run', 'FunctionCall', \
               'v8.callFunction', 'RunMicrotasks' \
             ) OR \
             s.name GLOB '*ExecuteScript*' OR \
             s.name GLOB '*EvaluateScript*' OR \
             s.name GLOB '*ScriptRunner*' OR \
             s.name GLOB '*TimerFire*' OR \
             s.name GLOB '*parseOnBackground*' OR \
             s.name GLOB '*RunScriptStreamingTask*' \
           )",
    );
    if let Some(bound) = script_start_bound {
        sql.push_str(&format!(" AND s.ts + s.dur > {bound}"));
    }
    if let Some(bound) = script_end_bound {
        sql.push_str(&format!(" AND s.ts < {bound}"));
    }
    if page_window.phase.is_some() {
        sql.push_str(" AND rw.start_ts IS NOT NULL AND rw.end_ts IS NOT NULL");
    }
    if let (Some(start), Some(end)) = (script_start_bound, script_end_bound) {
        sql.push_str(&format!(" AND {end} > {start}"));
    }
    sql.push_str(&format!(
        " GROUP BY s.id, s.ts, s.dur, s.arg_set_id, s.thread_dur, s.name, p.name, p.upid, p.pid, t.name \
         ), \
         raw_script_url_args AS ( \
           SELECT \
             rss.id, \
             a.display_value AS url, \
             {script_url_arg_priority_expr} AS url_priority \
           FROM raw_script_slice_base rss \
           JOIN args a ON rss.arg_set_id = a.arg_set_id \
           WHERE a.display_value IS NOT NULL \
             AND a.display_value != '' \
         ), \
         raw_script_min_url_priority AS ( \
           SELECT id, MIN(url_priority) AS url_priority \
           FROM raw_script_url_args \
           WHERE url_priority < 99 \
           GROUP BY id \
         ), \
         raw_script_selected_urls AS ( \
           SELECT rsu.id, MIN(rsu.url) AS url \
           FROM raw_script_url_args rsu \
           JOIN raw_script_min_url_priority rsup \
             ON rsup.id = rsu.id AND rsup.url_priority = rsu.url_priority \
           GROUP BY rsu.id \
         ), \
         raw_script_slices AS ( \
           SELECT DISTINCT \
             rss.id, \
             rss.ts, \
             rss.dur, \
             rss.thread_dur, \
             rss.overlap_start_ts, \
             rss.overlap_end_ts, \
             rss.overlap_dur, \
             rss.overlap_thread_dur, \
             rss.name, \
             rss.start_ms, \
             rss.end_ms, \
             rss.process_name, \
             rss.upid, \
             rss.pid, \
             rss.thread_name, \
             rssu.url, \
             rss.size_bytes \
           FROM raw_script_slice_base rss \
           JOIN raw_script_selected_urls rssu ON rssu.id = rss.id \
         ), \
         script_slices AS ( \
           SELECT raw_script_slices.* \
           FROM raw_script_slices \
           WHERE {url_key_expr} IN (SELECT url_key FROM matched_url_keys) \
          ), \
         script_descendants(root_id, id, depth) AS ( \
           SELECT id, id, 0 FROM script_slices \
           UNION ALL \
           SELECT sd.root_id, child.id, sd.depth + 1 \
           FROM script_descendants sd \
           JOIN slice child ON child.parent_id = sd.id \
           WHERE sd.depth < 8 \
             AND child.dur > 0 \
             AND NOT EXISTS ( \
               SELECT 1 FROM script_slices nested_root \
               WHERE nested_root.id = child.id \
             ) \
         ), \
          script_descendant_rollup AS ( \
            SELECT \
              {ss_url_key_expr} AS url_key, \
              SUM(CASE WHEN d.depth > 0 AND ( \
                child.name GLOB '*ForcedStyle*' OR \
                child.name = 'Blink.ForcedStyleAndLayout.UpdateTime' \
              ) THEN CASE WHEN child.ts + child.dur > ss.overlap_start_ts \
                            AND child.ts < ss.overlap_end_ts \
                          THEN MIN(child.ts + child.dur, ss.overlap_end_ts) \
                               - MAX(child.ts, ss.overlap_start_ts) \
                          ELSE 0 END \
                ELSE 0 END) AS forced_style_layout_ns, \
              SUM(CASE WHEN d.depth > 0 AND ( \
                child.name GLOB '*Style*' OR \
                child.name = 'Blink.Style.UpdateTime' \
              ) AND NOT ( \
                child.name GLOB '*ForcedStyle*' OR \
                child.name = 'Blink.ForcedStyleAndLayout.UpdateTime' \
              ) THEN CASE WHEN child.ts + child.dur > ss.overlap_start_ts \
                            AND child.ts < ss.overlap_end_ts \
                          THEN MIN(child.ts + child.dur, ss.overlap_end_ts) \
                               - MAX(child.ts, ss.overlap_start_ts) \
                          ELSE 0 END \
                ELSE 0 END) AS style_recalc_ns, \
              SUM(CASE WHEN d.depth > 0 AND ( \
                child.name GLOB '*Layout*' OR \
                child.name = 'Blink.Layout.UpdateTime' OR \
                child.name = 'Layout' \
              ) AND NOT ( \
                child.name GLOB '*ForcedStyle*' OR \
                child.name = 'Blink.ForcedStyleAndLayout.UpdateTime' \
              ) AND NOT ( \
                child.name GLOB '*Style*' OR \
                child.name = 'Blink.Style.UpdateTime' \
              ) THEN CASE WHEN child.ts + child.dur > ss.overlap_start_ts \
                            AND child.ts < ss.overlap_end_ts \
                          THEN MIN(child.ts + child.dur, ss.overlap_end_ts) \
                               - MAX(child.ts, ss.overlap_start_ts) \
                          ELSE 0 END \
                ELSE 0 END) AS layout_ns \
            FROM script_descendants d \
            JOIN slice child ON child.id = d.id \
            JOIN script_slices ss ON ss.id = d.root_id \
            GROUP BY {ss_url_key_expr} \
         ), \
         resource_rollup AS ( \
           SELECT \
              rr.url_key, \
              (SELECT r2.url FROM resource_rows r2 \
               WHERE r2.url_key = rr.url_key \
               ORDER BY r2.overlap_dur DESC, r2.dur DESC, r2.id ASC LIMIT 1) AS example_url, \
              (SELECT r2.matched_by FROM resource_rows r2 \
               WHERE r2.url_key = rr.url_key \
               ORDER BY CASE r2.matched_by \
                         WHEN 'url_substring+example_slice_id' THEN 0 \
                         WHEN 'url_substring' THEN 1 \
                         ELSE 2 END, \
                        r2.overlap_dur DESC, r2.dur DESC, r2.id ASC LIMIT 1) AS matched_by, \
              (SELECT r2.matched_url_seed FROM resource_rows r2 \
               WHERE r2.url_key = rr.url_key \
               ORDER BY CASE r2.matched_by \
                         WHEN 'url_substring+example_slice_id' THEN 0 \
                         WHEN 'url_substring' THEN 1 \
                         ELSE 2 END, \
                        r2.overlap_dur DESC, r2.dur DESC, r2.id ASC LIMIT 1) AS matched_url_seed, \
              COUNT(*) AS resource_slice_count, \
             SUM(CASE WHEN rr.slice_duration_status = 'incomplete_duration' THEN 1 ELSE 0 END) \
               AS incomplete_duration_resource_slice_count, \
             ROUND(MIN(rr.start_ms), 3) AS first_resource_start_ms, \
             ROUND(MAX(rr.end_ms), 3) AS last_resource_end_ms, \
             ROUND(MAX(rr.overlap_dur) / 1e6, 3) AS max_request_overlap_ms, \
             ROUND(MAX(CASE WHEN rr.name IN ( \
               'ScheduledResourceRequest', 'URL_REQUEST_START_JOB', \
               'REQUEST_ALIVE', 'CORS_REQUEST' \
             ) THEN rr.overlap_dur END) / 1e6, 3) AS request_span_ms, \
             ROUND(MIN(CASE WHEN rr.name = 'Resource::Create' \
               THEN rr.start_ms END), 3) AS resource_create_ms, \
             ROUND(MIN(CASE WHEN rr.name GLOB '*OnReceiveResponse*' \
               OR rr.name GLOB '*SendResponseToClient*' \
               THEN rr.start_ms END), 3) AS response_start_ms, \
             ROUND(MAX(CASE WHEN rr.name GLOB '*Cache*' \
               OR rr.name GLOB '*GetResource*' \
               THEN rr.overlap_dur END) / 1e6, 3) AS cache_or_get_resource_span_ms, \
             (SELECT r2.id FROM resource_rows r2 \
              WHERE r2.url_key = rr.url_key \
              ORDER BY r2.overlap_dur DESC, r2.dur DESC, r2.id ASC LIMIT 1) AS example_resource_slice_id \
           FROM resource_rows rr \
           GROUP BY rr.url_key \
         ), \
         script_rollup AS ( \
           SELECT \
             {ss_url_key_expr} AS url_key, \
             COUNT(*) AS script_slice_count, \
             ROUND(SUM(CASE WHEN ss.name GLOB '*parseOnBackground*' \
               OR ss.name GLOB '*RunScriptStreamingTask*' THEN ss.overlap_dur ELSE 0 END) / 1e6, 3) \
               AS background_parse_ms, \
             ROUND(MAX(CASE WHEN ss.name IN ('EvaluateScript', 'v8.run') \
               OR ss.name GLOB '*EvaluateScript*' THEN ss.overlap_dur END) / 1e6, 3) \
               AS max_evaluate_ms, \
             ROUND(SUM(CASE WHEN ss.name IN ('EvaluateScript', 'v8.run') \
               OR ss.name GLOB '*EvaluateScript*' THEN ss.overlap_dur ELSE 0 END) / 1e6, 3) \
               AS total_evaluate_ms, \
             ROUND(MAX(CASE WHEN ss.name IN ('FunctionCall', 'v8.callFunction', 'RunMicrotasks') \
               THEN ss.overlap_dur END) / 1e6, 3) AS max_callback_ms, \
             ROUND(MAX(ss.overlap_thread_dur) / 1e6, 3) AS max_script_cpu_ms, \
             MAX(ss.size_bytes) AS size_bytes, \
             ROUND(MAX(COALESCE(sdr.forced_style_layout_ns, 0)) / 1e6, 3) AS forced_style_layout_ms, \
             ROUND(MAX(COALESCE(sdr.style_recalc_ns, 0)) / 1e6, 3) AS style_recalc_ms, \
             ROUND(MAX(COALESCE(sdr.layout_ns, 0)) / 1e6, 3) AS layout_ms, \
             (SELECT s2.id FROM script_slices s2 \
              WHERE {s2_url_key_expr} = {ss_url_key_expr} \
              ORDER BY s2.overlap_dur DESC, s2.dur DESC, s2.id ASC LIMIT 1) AS example_script_slice_id \
            FROM script_slices ss \
            LEFT JOIN script_descendant_rollup sdr ON sdr.url_key = {ss_url_key_expr} \
           GROUP BY {ss_url_key_expr} \
         ) \
         SELECT \
           rr.url_key, \
           rr.example_url, \
           rr.matched_by, \
           rr.matched_url_seed, \
           rr.resource_slice_count, \
           COALESCE(sr.script_slice_count, 0) AS script_slice_count, \
           rr.first_resource_start_ms, \
           rr.last_resource_end_ms, \
           rr.max_request_overlap_ms, \
           rr.request_span_ms, \
           rr.resource_create_ms, \
           rr.response_start_ms, \
           rr.cache_or_get_resource_span_ms, \
           sr.background_parse_ms, \
           sr.max_evaluate_ms, \
           sr.total_evaluate_ms, \
           sr.max_callback_ms, \
           sr.max_script_cpu_ms, \
           sr.forced_style_layout_ms, \
           sr.style_recalc_ms, \
           sr.layout_ms, \
           sr.size_bytes, \
           rr.example_resource_slice_id, \
           sr.example_script_slice_id, \
           'fact: lifecycle/request spans and script slices; hypothesis: DNS/TLS/TTFB/download/cache/CDN unless phase-specific rows are inspected' \
             AS evidence_boundary \
         FROM resource_rollup rr \
         LEFT JOIN script_rollup sr ON sr.url_key = rr.url_key \
         ORDER BY rr.max_request_overlap_ms DESC, rr.first_resource_start_ms ASC \
         LIMIT {row_limit}"
    ));
    Ok(sql)
}

fn chrome_resource_url_key_expr(
    url_expr: &str,
    grouping: Option<ChromePageLoadResourceUrlGrouping>,
) -> String {
    match grouping.unwrap_or(ChromePageLoadResourceUrlGrouping::Full) {
        ChromePageLoadResourceUrlGrouping::Full => url_expr.to_owned(),
        ChromePageLoadResourceUrlGrouping::WithoutQuery => format!(
            "CASE WHEN INSTR({url_expr}, '?') > 0 \
                  THEN SUBSTR({url_expr}, 1, INSTR({url_expr}, '?') - 1) \
                  ELSE {url_expr} END"
        ),
    }
}

pub(super) fn chrome_url_arg_priority_expr(args_alias: &str) -> String {
    let flat_key = format!("{args_alias}.flat_key");
    let key = format!("{args_alias}.key");
    let value = format!("{args_alias}.display_value");
    format!(
        "CASE \
           WHEN LOWER({value}) IN ('http://unisolated.invalid/', 'https://unisolated.invalid/') \
             THEN 90 \
           WHEN LOWER({flat_key}) IN ( \
             'debug.url', 'debug.data.url', \
             'debug.data.request_url', 'debug.data.script_url', \
             'debug.filename', 'debug.navigation_request.url', \
             'debug.initial url', 'page_load.url', \
             'url', 'request_url', 'script_url', 'filename' \
           ) \
             OR LOWER({key}) IN ('url', 'request_url', 'script_url', 'filename') \
             THEN 1 \
           WHEN LOWER({flat_key}) GLOB '*current_frame_host.url' \
             OR LOWER({flat_key}) GLOB '*current_frame.url' \
             OR LOWER({flat_key}) GLOB '*frame.url' \
             THEN 2 \
           WHEN LOWER({flat_key}) GLOB '*process_lock_url' \
             OR LOWER({flat_key}) GLOB '*site_url' \
             OR LOWER({key}) GLOB '*process_lock_url' \
             OR LOWER({key}) GLOB '*site_url' \
             THEN 8 \
           WHEN LOWER({flat_key}) GLOB '*request*url*' \
             OR LOWER({flat_key}) GLOB '*script*url*' \
             OR LOWER({key}) GLOB '*request*url*' \
             OR LOWER({key}) GLOB '*script*url*' \
             THEN 3 \
           WHEN LOWER({flat_key}) LIKE '%url%' \
             OR LOWER({key}) LIKE '%url%' \
             THEN 5 \
           ELSE 99 \
         END"
    )
}

const RAW_WINDOW_NAV_OBSERVED_END_EXPR: &str = "COALESCE(NULLIF(MAX( \
        COALESCE(load_event_ts, -1), \
        COALESCE(fcp_ts, -1), \
        COALESCE(lcp_ts, -1), \
        COALESCE(dom_content_loaded_event_ts, -1), \
        COALESCE(mark_fully_loaded_ts, -1), \
        COALESCE(mark_fully_visible_ts, -1), \
        COALESCE(mark_interactive_ts, -1) \
    ), -1), trace_end())";

fn chrome_page_load_raw_window_navigation_url_expr(page_window: ChromePageLoadWindowSql) -> String {
    let Some(predicates) = chrome_page_load_raw_window_navigation_predicates(page_window) else {
        return "(SELECT url FROM chrome_page_loads ORDER BY navigation_start_ts DESC LIMIT 1)"
            .to_owned();
    };

    format!(
        "(SELECT url FROM chrome_page_loads \
         WHERE {} \
         ORDER BY navigation_start_ts DESC LIMIT 1)",
        predicates.join(" AND ")
    )
}

fn chrome_page_load_raw_window_navigation_count_expr(
    page_window: ChromePageLoadWindowSql,
) -> String {
    let Some(predicates) = chrome_page_load_raw_window_navigation_predicates(page_window) else {
        return "1".to_owned();
    };
    format!(
        "(SELECT COUNT(*) FROM chrome_page_loads WHERE {})",
        predicates.join(" AND ")
    )
}

fn chrome_page_load_raw_window_navigation_status_expr(
    page_window: ChromePageLoadWindowSql,
) -> String {
    let Some(_) = chrome_page_load_raw_window_navigation_predicates(page_window) else {
        return "'latest_navigation_fallback'".to_owned();
    };
    let count_expr = chrome_page_load_raw_window_navigation_count_expr(page_window);
    format!(
        "CASE \
           WHEN {count_expr} = 0 THEN 'none' \
           WHEN {count_expr} = 1 THEN 'single' \
           ELSE 'ambiguous' \
         END"
    )
}

fn chrome_page_load_raw_window_navigation_predicates(
    page_window: ChromePageLoadWindowSql,
) -> Option<Vec<String>> {
    if page_window.start_ts_ns.is_none() && page_window.end_ts_ns.is_none() {
        return None;
    }
    let mut predicates = vec!["navigation_start_ts IS NOT NULL".to_owned()];
    if let Some(end) = page_window.end_ts_ns {
        predicates.push(format!("navigation_start_ts < {end}"));
    }
    if let Some(start) = page_window.start_ts_ns {
        predicates.push(format!("{RAW_WINDOW_NAV_OBSERVED_END_EXPR} > {start}"));
    }
    Some(predicates)
}

fn chrome_resource_url_authority_expr(url_expr: &str) -> String {
    let rest = format!("SUBSTR({url_expr}, INSTR({url_expr}, '://') + 3)");
    format!(
        "CASE \
           WHEN INSTR({rest}, '/') = 0 \
             AND INSTR({rest}, '?') = 0 \
             AND INSTR({rest}, '#') = 0 THEN {rest} \
           WHEN INSTR({rest}, '/') > 0 \
             AND (INSTR({rest}, '?') = 0 OR INSTR({rest}, '/') < INSTR({rest}, '?')) \
             AND (INSTR({rest}, '#') = 0 OR INSTR({rest}, '/') < INSTR({rest}, '#')) \
             THEN SUBSTR({rest}, 1, INSTR({rest}, '/') - 1) \
           WHEN INSTR({rest}, '?') > 0 \
             AND (INSTR({rest}, '#') = 0 OR INSTR({rest}, '?') < INSTR({rest}, '#')) \
             THEN SUBSTR({rest}, 1, INSTR({rest}, '?') - 1) \
           WHEN INSTR({rest}, '#') > 0 \
             THEN SUBSTR({rest}, 1, INSTR({rest}, '#') - 1) \
           ELSE {rest} \
         END"
    )
}

fn chrome_resource_url_host_expr(url_expr: &str) -> String {
    let authority_expr = chrome_resource_url_authority_expr(url_expr);
    format!(
        "CASE \
           WHEN INSTR({url_expr}, '://') > 0 THEN LOWER({authority_expr}) \
           ELSE '' \
         END"
    )
}

pub(super) fn chrome_resource_url_origin_expr(url_expr: &str) -> String {
    let authority_expr = chrome_resource_url_authority_expr(url_expr);
    format!(
        "CASE \
           WHEN INSTR({url_expr}, '://') > 0 THEN \
             LOWER(SUBSTR({url_expr}, 1, INSTR({url_expr}, '://') + 2) || {authority_expr}) \
           ELSE '' \
         END"
    )
}

/// SQL builder for resource-timing evidence metadata returned with
/// `chrome_page_load_resource_summary`.
///
/// This is deliberately a capability probe rather than a HAR reconstruction:
/// when phase-like slice names or arg keys are absent, LLM callers should keep
/// resource-summary conclusions at "URL lifecycle span" level and avoid naming
/// DNS/TLS/TTFB/download/cache as the proven bottleneck.
pub fn chrome_page_load_resource_timing_evidence_sql(
    window: ChromePageLoadWindowFilters,
) -> Result<String, PerfettoError> {
    let page_window = validate_chrome_page_load_window(window)?;
    let mut sql = String::new();
    append_chrome_page_load_window_cte(&mut sql, "resource_window", window, page_window);
    if sql.is_empty() {
        sql.push_str("WITH ");
    } else {
        sql.push_str(", ");
    }

    let exprs = chrome_resource_window_exprs(page_window);
    sql.push_str(
        "resource_timing_probe AS ( \
           SELECT \
             COUNT(DISTINCT CASE WHEN ( \
               lower(s.name) GLOB '*dns*' OR \
               lower(s.name) GLOB '*connect*' OR \
               lower(s.name) GLOB '*socket*' OR \
               lower(s.name) GLOB '*ssl*' OR \
               lower(s.name) GLOB '*tls*' OR \
               lower(s.name) GLOB '*http*' OR \
               lower(s.name) GLOB '*cache*' OR \
               lower(s.name) GLOB '*receive*' OR \
               lower(s.name) GLOB '*download*' OR \
               lower(s.name) GLOB '*ttfb*' \
             ) THEN s.id END) AS network_phase_slice_count, \
             COUNT(DISTINCT CASE WHEN ( \
               lower(a.flat_key) GLOB '*dns*' OR \
               lower(a.flat_key) GLOB '*connect*' OR \
               lower(a.flat_key) GLOB '*socket*' OR \
               lower(a.flat_key) GLOB '*ssl*' OR \
               lower(a.flat_key) GLOB '*tls*' OR \
               lower(a.flat_key) GLOB '*http*' OR \
               lower(a.flat_key) GLOB '*cache*' OR \
               lower(a.flat_key) GLOB '*receive*' OR \
               lower(a.flat_key) GLOB '*download*' OR \
               lower(a.flat_key) GLOB '*ttfb*' \
             ) THEN s.arg_set_id END) AS network_phase_arg_count, \
             COUNT(DISTINCT CASE WHEN s.dur < 0 \
               AND ( \
                 s.name GLOB '*Resource*' OR \
                 s.name GLOB '*URLLoader*' OR \
                 s.name GLOB '*URLRequest*' OR \
                 s.name GLOB '*Network*' OR \
                 s.name GLOB '*Request*' OR \
                 s.name GLOB '*Fetch*' OR \
                 s.name GLOB '*XHR*' \
               ) \
               AND ( \
                 a.flat_key IN ( \
                   'debug.url', 'debug.data.url', 'debug.data.request_url', \
                   'debug.fileName', 'url', 'request_url' \
                 ) OR \
                 a.key IN ('url', 'request_url', 'fileName') OR \
                 lower(a.flat_key) LIKE '%url%' \
               ) \
             THEN s.id END) AS incomplete_duration_resource_slice_count \
           FROM slice s \
           LEFT JOIN args a ON s.arg_set_id = a.arg_set_id ",
    );
    if page_window.phase.is_some() {
        sql.push_str("CROSS JOIN resource_window rw ");
    }
    sql.push_str(
        "WHERE ( \
             lower(s.name) GLOB '*dns*' OR \
             lower(s.name) GLOB '*connect*' OR \
             lower(s.name) GLOB '*socket*' OR \
             lower(s.name) GLOB '*ssl*' OR \
             lower(s.name) GLOB '*tls*' OR \
             lower(s.name) GLOB '*http*' OR \
             lower(s.name) GLOB '*cache*' OR \
             lower(s.name) GLOB '*receive*' OR \
             lower(s.name) GLOB '*download*' OR \
             lower(s.name) GLOB '*ttfb*' OR \
             lower(a.flat_key) GLOB '*dns*' OR \
             lower(a.flat_key) GLOB '*connect*' OR \
             lower(a.flat_key) GLOB '*socket*' OR \
             lower(a.flat_key) GLOB '*ssl*' OR \
             lower(a.flat_key) GLOB '*tls*' OR \
             lower(a.flat_key) GLOB '*http*' OR \
             lower(a.flat_key) GLOB '*cache*' OR \
             lower(a.flat_key) GLOB '*receive*' OR \
             lower(a.flat_key) GLOB '*download*' OR \
             lower(a.flat_key) GLOB '*ttfb*' OR \
             s.name GLOB '*Resource*' OR \
             s.name GLOB '*URLLoader*' OR \
             s.name GLOB '*URLRequest*' OR \
             s.name GLOB '*Network*' OR \
             s.name GLOB '*Request*' OR \
             s.name GLOB '*Fetch*' OR \
             s.name GLOB '*XHR*' \
           )",
    );
    if let Some(bound) = &exprs.start_bound {
        sql.push_str(&format!(
            " AND ((s.dur >= 0 AND s.ts + s.dur > {bound}) OR (s.dur < 0 AND s.ts >= {bound}))"
        ));
    }
    if let Some(bound) = &exprs.end_bound {
        sql.push_str(&format!(" AND s.ts < {bound}"));
    }
    if page_window.phase.is_some() {
        sql.push_str(" AND rw.start_ts IS NOT NULL AND rw.end_ts IS NOT NULL");
    }
    if let (Some(start), Some(end)) = (&exprs.start_bound, &exprs.end_bound) {
        sql.push_str(&format!(" AND {end} > {start}"));
    }
    sql.push_str(
        ") \
         SELECT \
           COALESCE(network_phase_slice_count, 0) AS network_phase_slice_count, \
           COALESCE(network_phase_arg_count, 0) AS network_phase_arg_count, \
           COALESCE(incomplete_duration_resource_slice_count, 0) \
             AS incomplete_duration_resource_slice_count, \
           CASE WHEN COALESCE(network_phase_slice_count, 0) \
                   + COALESCE(network_phase_arg_count, 0) > 0 \
                THEN 1 ELSE 0 END AS phase_breakdown_available \
         FROM resource_timing_probe",
    );
    Ok(sql)
}
