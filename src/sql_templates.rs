// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use crate::error::{PerfettoError, MAX_ROWS};
use crate::params::{
    ChromeMainThreadHotspotsFilters, ChromePageLoadPhase, ChromePageLoadResourceHotspotsFilters,
    ChromePageLoadResourcePipelineFilters, ChromePageLoadResourceSummaryFilters,
    ChromePageLoadResourceUrlGrouping, ChromePageLoadScriptHotspotsFilters,
    ChromePageLoadWindowFilters, SliceDescendantsBreakdownFilters,
};

/// Trace-level metadata used by `load_trace` to avoid leaving callers in a
/// routing vacuum after a successful load. Kept intentionally small: selected
/// metadata keys plus cheap scalar probes.
pub const LOAD_TRACE_METADATA_SQL: &str = "SELECT name, str_value, int_value \
     FROM metadata \
     WHERE name IN ( \
       'trace_type', \
       'system_name', \
       'system_machine', \
       'android_build_fingerprint', \
       'android_sdk_version', \
       'cr-os-name', \
       'cr-2-os-name', \
       'cr-product-version', \
       'cr-2-product-version' \
     ) \
     ORDER BY name";

/// Scalar overview for `load_trace` routing hints.
///
/// `trace_start()` / `trace_end()` / `trace_dur()` expose trace_processor's
/// capture interval; they are a better default than deriving duration from
/// slices because traces can contain sparse or no slices. `EXISTS` probes stop
/// at the first row and avoid materializing large tables.
pub const LOAD_TRACE_OVERVIEW_SQL: &str = "SELECT \
       trace_start() AS start_ts, \
       trace_end() AS end_ts, \
       trace_dur() AS duration_ns, \
       (SELECT COUNT(*) FROM process) AS process_count, \
       (SELECT COUNT(*) FROM thread) AS thread_count, \
       EXISTS(SELECT 1 FROM slice) AS has_slices, \
       EXISTS(SELECT 1 FROM counter) AS has_counters, \
       EXISTS(SELECT 1 FROM sched) AS has_sched, \
       EXISTS(SELECT 1 FROM ftrace_event) AS has_ftrace, \
       EXISTS(SELECT 1 FROM args WHERE flat_key = 'chrome.process_type') AS has_chrome";

pub const DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS: f64 = 1.0;
pub const DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH: u32 = 8;
pub const DEFAULT_SLICE_DESCENDANTS_LIMIT: u32 = 100;
pub const MAX_SLICE_DESCENDANTS_ROOTS: usize = 100;

/// SQL for chrome_scroll_jank_summary. Exported for integration tests.
/// Returns row-level janky frames (not pre-aggregated) so agents can do
/// their own grouping, correlation, and deep-dive queries after the first call.
pub const CHROME_SCROLL_JANK_SUMMARY_SQL: &str =
    "INCLUDE PERFETTO MODULE chrome.scroll_jank.scroll_jank_v3; \
     SELECT \
       cause_of_jank, \
       sub_cause_of_jank, \
       delay_since_last_frame, \
       event_latency_id, \
       scroll_id, \
       vsync_interval \
     FROM chrome_janky_frames \
     ORDER BY delay_since_last_frame DESC \
     LIMIT 100";

/// SQL for chrome_page_load_summary. Exported for integration tests.
pub const CHROME_PAGE_LOAD_SUMMARY_SQL: &str = "INCLUDE PERFETTO MODULE chrome.page_loads; \
     SELECT \
       id, \
       navigation_id, \
       url, \
       navigation_start_ts, \
       fcp_ts, \
       dom_content_loaded_event_ts, \
       load_event_ts, \
       fcp / 1e6 AS fcp_ms, \
       lcp / 1e6 AS lcp_ms, \
       CASE WHEN dom_content_loaded_event_ts IS NOT NULL \
            THEN (dom_content_loaded_event_ts - navigation_start_ts) / 1e6 \
       END AS dcl_ms, \
       CASE WHEN load_event_ts IS NOT NULL \
            THEN (load_event_ts - navigation_start_ts) / 1e6 \
       END AS load_ms \
     FROM chrome_page_loads \
     ORDER BY navigation_start_ts DESC \
     LIMIT 100";

#[derive(Debug, Clone, Copy)]
struct ChromePageLoadWindowSql {
    phase: Option<ChromePageLoadPhase>,
    start_ts_ns: Option<i64>,
    end_ts_ns: Option<i64>,
}

fn validate_chrome_page_load_window(
    filters: ChromePageLoadWindowFilters,
) -> Result<ChromePageLoadWindowSql, PerfettoError> {
    let ChromePageLoadWindowFilters {
        page_load_id,
        navigation_id,
        phase,
        start_ts_ns,
        end_ts_ns,
    } = filters;

    if let Some(id) = page_load_id {
        if id < 0 {
            return Err(PerfettoError::InvalidParam(format!(
                "page_load_id must be non-negative, got {id}"
            )));
        }
    }
    if let Some(id) = navigation_id {
        if id < 0 {
            return Err(PerfettoError::InvalidParam(format!(
                "navigation_id must be non-negative, got {id}"
            )));
        }
    }
    if page_load_id.is_some() && navigation_id.is_some() {
        return Err(PerfettoError::InvalidParam(
            "page_load_id and navigation_id are mutually exclusive".to_owned(),
        ));
    }
    if let Some(ts) = start_ts_ns {
        if ts < 0 {
            return Err(PerfettoError::InvalidParam(format!(
                "start_ts_ns must be non-negative, got {ts}"
            )));
        }
    }
    if let Some(ts) = end_ts_ns {
        if ts < 0 {
            return Err(PerfettoError::InvalidParam(format!(
                "end_ts_ns must be non-negative, got {ts}"
            )));
        }
    }
    if let (Some(start), Some(end)) = (start_ts_ns, end_ts_ns) {
        if end <= start {
            return Err(PerfettoError::InvalidParam(format!(
                "end_ts_ns must be greater than start_ts_ns, got start={start}, end={end}"
            )));
        }
    }

    let phase = phase.or_else(|| {
        (page_load_id.is_some() || navigation_id.is_some())
            .then_some(ChromePageLoadPhase::NavigationToFcp)
    });

    Ok(ChromePageLoadWindowSql {
        phase,
        start_ts_ns,
        end_ts_ns,
    })
}

fn chrome_page_load_phase_columns(phase: ChromePageLoadPhase) -> (&'static str, &'static str) {
    match phase {
        ChromePageLoadPhase::NavigationToFcp => ("navigation_start_ts", "fcp_ts"),
        ChromePageLoadPhase::NavigationToLoad => ("navigation_start_ts", "load_event_ts"),
        ChromePageLoadPhase::DclToFcp => ("dom_content_loaded_event_ts", "fcp_ts"),
        ChromePageLoadPhase::FcpToLoad => ("fcp_ts", "load_event_ts"),
    }
}

fn append_chrome_page_load_window_cte(
    sql: &mut String,
    cte_name: &str,
    filters: ChromePageLoadWindowFilters,
    window: ChromePageLoadWindowSql,
) {
    if let Some(phase) = window.phase {
        sql.push_str("INCLUDE PERFETTO MODULE chrome.page_loads; ");
        sql.push_str("WITH ");
        append_chrome_page_load_window_cte_body(sql, cte_name, filters, phase);
    }
}

fn append_chrome_page_load_window_cte_body(
    sql: &mut String,
    cte_name: &str,
    filters: ChromePageLoadWindowFilters,
    phase: ChromePageLoadPhase,
) {
    let (start_expr, end_expr) = chrome_page_load_phase_columns(phase);
    sql.push_str(&format!(
        "{cte_name} AS ( \
         SELECT \
           navigation_start_ts AS navigation_start_ts, \
           url AS url, \
           {start_expr} AS start_ts, \
           {end_expr} AS end_ts, \
           ({end_expr} - {start_expr}) AS phase_dur_ns \
         FROM chrome_page_loads "
    ));
    if let Some(id) = filters.page_load_id {
        sql.push_str(&format!("WHERE id = {id} "));
    }
    if let Some(id) = filters.navigation_id {
        sql.push_str(&format!("WHERE navigation_id = {id} "));
    }
    sql.push_str("ORDER BY navigation_start_ts DESC LIMIT 1) ");
}

fn duration_ms_to_ns(
    field_name: &str,
    value_ms: Option<f64>,
    default_ns: i64,
) -> Result<i64, PerfettoError> {
    match value_ms {
        None => Ok(default_ns),
        Some(ms) => {
            let ns = ms * 1_000_000.0;
            if !(ns.is_finite() && ns >= 0.0 && ns <= i64::MAX as f64) {
                return Err(PerfettoError::InvalidParam(format!(
                    "{field_name} must be finite, non-negative, and ≤ ~9.2e12 ms, got {ms}"
                )));
            }
            Ok(ns as i64)
        }
    }
}

fn chrome_tool_row_limit(limit: Option<u32>) -> Result<u32, PerfettoError> {
    chrome_tool_row_limit_with_default(limit, 100)
}

fn chrome_tool_row_limit_with_default(
    limit: Option<u32>,
    default_limit: u32,
) -> Result<u32, PerfettoError> {
    match limit {
        None => Ok(default_limit),
        Some(0) => Err(PerfettoError::InvalidParam("limit must be > 0".to_owned())),
        Some(n) if (n as usize) > MAX_ROWS => Ok(MAX_ROWS as u32),
        Some(n) => Ok(n),
    }
}

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
        Some(bound) => format!("MIN(s.ts + s.dur, {bound})"),
        None => "s.ts + s.dur".to_owned(),
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
             ROUND((s.ts + s.dur - {}) / 1e6, 3) AS end_ms, \
             ROUND(s.dur / 1e6, 3) AS dur_ms, \
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
        "WHERE s.dur >= {min_dur_ns} \
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
           )"
    ));
    if let Some(bound) = &exprs.start_bound {
        sql.push_str(&format!(" AND s.ts + s.dur > {bound}"));
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

/// SQL builder for `chrome_main_thread_hotspots`. Exported for integration tests.
///
/// Uses `thread.is_main_thread = 1` when trace_processor populated it, plus
/// Chrome's `Cr*Main` thread-name convention as a fallback for Chromium-family
/// traces that carry main-thread names but do not set the flag correctly.
///
/// All set filter clauses AND together — the redundancy is harmless (e.g.
/// `upid=3 AND pid=12800` still hits when the pair refers to one process).
/// The base SQL picks up a `LEFT JOIN process p ON ct.upid = p.upid` so `p.pid`
/// and `p.upid` are referenceable; the join is harmless when no process
/// filter is present. `ChromeMainThreadHotspotsFilters::default()` is
/// equivalent to the default tool behavior.
pub fn chrome_main_thread_hotspots_sql(
    filters: ChromeMainThreadHotspotsFilters<'_>,
) -> Result<String, PerfettoError> {
    let ChromeMainThreadHotspotsFilters {
        process_name,
        pid,
        upid,
        page_load_id,
        navigation_id,
        phase,
        start_ts_ns,
        end_ts_ns,
        min_dur_ms,
        limit,
    } = filters;
    let page_window_filters = ChromePageLoadWindowFilters {
        page_load_id,
        navigation_id,
        phase,
        start_ts_ns,
        end_ts_ns,
    };
    let page_window = validate_chrome_page_load_window(page_window_filters)?;
    let min_dur_ns = duration_ms_to_ns("min_dur_ms", min_dur_ms, 16_000_000)?;
    let row_limit = chrome_tool_row_limit(limit)?;
    let effective_phase = page_window.phase;
    let mut sql = String::from("INCLUDE PERFETTO MODULE chrome.tasks; ");
    append_chrome_page_load_window_cte(
        &mut sql,
        "hotspot_window",
        page_window_filters,
        page_window,
    );
    sql.push_str(
        "SELECT \
           ct.id, \
           ct.ts, \
           ct.name, \
           ct.task_type, \
           ct.thread_name, \
           ct.process_name, \
           ct.upid, \
           p.pid, \
           ct.dur / 1e6 AS dur_ms, \
           CASE WHEN ct.thread_dur IS NOT NULL AND ct.dur > 0 \
                THEN ROUND(ct.thread_dur * 100.0 / ct.dur, 1) \
           END AS cpu_pct, \
           ct.thread_dur / 1e6 AS thread_dur_ms \
         FROM chrome_tasks ct \
         LEFT JOIN thread t ON ct.utid = t.utid \
         LEFT JOIN process p ON ct.upid = p.upid ",
    );
    if effective_phase.is_some() {
        sql.push_str("CROSS JOIN hotspot_window hw ");
    }
    sql.push_str(&format!(
        "WHERE (t.is_main_thread = 1 OR ct.thread_name GLOB 'Cr*Main') \
           AND ct.dur > {min_dur_ns}"
    ));
    if effective_phase.is_some() {
        sql.push_str(
            " AND hw.start_ts IS NOT NULL \
              AND hw.end_ts IS NOT NULL \
              AND ct.ts >= hw.start_ts \
              AND ct.ts < hw.end_ts",
        );
    }
    if let Some(start) = page_window.start_ts_ns {
        sql.push_str(&format!(" AND ct.ts >= {start}"));
    }
    if let Some(end) = page_window.end_ts_ns {
        sql.push_str(&format!(" AND ct.ts < {end}"));
    }
    if let Some(name) = process_name {
        let lit = sql_string_literal(name)?;
        sql.push_str(&format!(" AND ct.process_name = {lit}"));
    }
    if let Some(pid) = pid {
        sql.push_str(&format!(" AND p.pid = {pid}"));
    }
    if let Some(upid) = upid {
        sql.push_str(&format!(" AND p.upid = {upid}"));
    }
    sql.push_str(&format!(" ORDER BY ct.dur DESC LIMIT {row_limit}"));
    Ok(sql)
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
            CASE \
              WHEN rr.url_key GLOB 'chrome://*' OR rr.url_key GLOB 'chrome-extension://*' \
                THEN 'browser_ui_or_extension' \
              WHEN (SELECT nav_url FROM navigation_context) IS NULL THEN 'unknown' \
              WHEN rr.url_key = {nav_url_key_expr} \
                THEN 'navigation_url' \
              WHEN {url_origin_expr} != '' AND {url_origin_expr} = {nav_origin_expr} \
                THEN 'same_origin' \
              WHEN {url_origin_expr} != '' THEN 'cross_origin' \
              ELSE 'unknown' \
            END AS relation_to_navigation, \
            CASE \
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
            (SELECT target_renderer_upids FROM navigation_context) AS target_renderer_upids, \
            COUNT(*) AS slice_count, \
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
                  THEN s.thread_dur * {script_overlap_dur_expr} * 1.0 / s.dur \
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
           WHERE sd.depth < 8 AND child.dur > 0 \
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
             ROUND(MIN(rr.start_ms), 3) AS first_resource_start_ms, \
             ROUND(MAX(rr.end_ms), 3) AS last_resource_end_ms, \
             ROUND(MAX(rr.overlap_dur) / 1e6, 3) AS max_request_overlap_ms, \
             ROUND(MAX(CASE WHEN rr.name IN ( \
               'ScheduledResourceRequest', 'URL_REQUEST_START_JOB', \
               'REQUEST_ALIVE', 'CORS_REQUEST' \
             ) THEN rr.dur END) / 1e6, 3) AS request_span_ms, \
             ROUND(MIN(CASE WHEN rr.name = 'Resource::Create' \
               THEN rr.start_ms END), 3) AS resource_create_ms, \
             ROUND(MIN(CASE WHEN rr.name GLOB '*OnReceiveResponse*' \
               OR rr.name GLOB '*SendResponseToClient*' \
               THEN rr.start_ms END), 3) AS response_start_ms, \
             ROUND(MAX(CASE WHEN rr.name GLOB '*Cache*' \
               OR rr.name GLOB '*GetResource*' \
               THEN rr.dur END) / 1e6, 3) AS cache_or_get_resource_span_ms, \
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

fn chrome_url_arg_priority_expr(args_alias: &str) -> String {
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

fn chrome_page_load_raw_window_navigation_url_expr(page_window: ChromePageLoadWindowSql) -> String {
    const NAV_END_EXPR: &str = "NULLIF(MAX( \
        COALESCE(load_event_ts, -1), \
        COALESCE(fcp_ts, -1), \
        COALESCE(lcp_ts, -1), \
        COALESCE(dom_content_loaded_event_ts, -1), \
        COALESCE(mark_fully_loaded_ts, -1), \
        COALESCE(mark_fully_visible_ts, -1), \
        COALESCE(mark_interactive_ts, -1) \
    ), -1)";

    if page_window.start_ts_ns.is_none() && page_window.end_ts_ns.is_none() {
        return "(SELECT url FROM chrome_page_loads ORDER BY navigation_start_ts DESC LIMIT 1)"
            .to_owned();
    }

    let mut predicates = vec![
        "navigation_start_ts IS NOT NULL".to_owned(),
        format!("{NAV_END_EXPR} IS NOT NULL"),
    ];
    if let Some(end) = page_window.end_ts_ns {
        predicates.push(format!("navigation_start_ts < {end}"));
    }
    if let Some(start) = page_window.start_ts_ns {
        predicates.push(format!("{NAV_END_EXPR} > {start}"));
    }

    format!(
        "(SELECT url FROM chrome_page_loads \
         WHERE {} \
         ORDER BY navigation_start_ts DESC LIMIT 1)",
        predicates.join(" AND ")
    )
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

fn chrome_resource_url_origin_expr(url_expr: &str) -> String {
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
             THEN s.id END) AS incomplete_resource_slice_count \
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
           COALESCE(incomplete_resource_slice_count, 0) AS incomplete_resource_slice_count, \
           CASE WHEN COALESCE(network_phase_slice_count, 0) \
                   + COALESCE(network_phase_arg_count, 0) > 0 \
                THEN 1 ELSE 0 END AS phase_breakdown_available \
         FROM resource_timing_probe",
    );
    Ok(sql)
}

/// SQL builder for `chrome_page_load_script_hotspots`.
///
/// Groups page-load-window script execution slices by URL/name/process/thread,
/// preserving repeated small callbacks that add up to significant wall time.
/// A bounded descendant walk annotates each group with style/layout work under
/// the script roots so agents can separate pure JS cost from synchronous render
/// pipeline work without hand-writing recursive CTEs.
pub fn chrome_page_load_script_hotspots_sql(
    filters: ChromePageLoadScriptHotspotsFilters<'_>,
) -> Result<String, PerfettoError> {
    let ChromePageLoadScriptHotspotsFilters {
        process_name,
        pid,
        upid,
        window,
        min_total_ms,
        limit,
    } = filters;
    let page_window = validate_chrome_page_load_window(window)?;
    let min_total_ns = duration_ms_to_ns("min_total_ms", min_total_ms, 20_000_000)?;
    let row_limit = chrome_tool_row_limit(limit)?;

    let start_bound = match (page_window.phase.is_some(), page_window.start_ts_ns) {
        (true, Some(ts)) => Some(format!("MAX(sw.start_ts, {ts})")),
        (true, None) => Some("sw.start_ts".to_owned()),
        (false, Some(ts)) => Some(ts.to_string()),
        (false, None) => None,
    };
    let end_bound = match (page_window.phase.is_some(), page_window.end_ts_ns) {
        (true, Some(ts)) => Some(format!("MIN(sw.end_ts, {ts})")),
        (true, None) => Some("sw.end_ts".to_owned()),
        (false, Some(ts)) => Some(ts.to_string()),
        (false, None) => None,
    };
    let anchor_expr = if page_window.phase.is_some() {
        "sw.navigation_start_ts".to_owned()
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
        Some(bound) => format!("MIN(s.ts + s.dur, {bound})"),
        None => "s.ts + s.dur".to_owned(),
    };
    let overlap_dur_expr = format!("({overlap_end_expr} - {overlap_start_expr})");

    let mut sql = String::new();
    if let Some(phase) = page_window.phase {
        sql.push_str("INCLUDE PERFETTO MODULE chrome.page_loads; WITH RECURSIVE ");
        append_chrome_page_load_window_cte_body(&mut sql, "script_window", window, phase);
        sql.push_str(", ");
    } else {
        sql.push_str("WITH RECURSIVE ");
    }

    sql.push_str(&format!(
        "script_slices AS ( \
           SELECT \
             s.id, \
             s.ts, \
             s.dur, \
             s.thread_dur, \
             {overlap_start_expr} AS overlap_start_ts, \
             {overlap_end_expr} AS overlap_end_ts, \
             {overlap_dur_expr} AS overlap_dur, \
             CASE WHEN s.thread_dur IS NOT NULL AND s.dur > 0 \
                  THEN s.thread_dur * {overlap_dur_expr} * 1.0 / s.dur \
             END AS overlap_thread_dur, \
             s.name, \
             ROUND(({overlap_start_expr} - {anchor_expr}) / 1e6, 3) AS start_ms, \
             ROUND(({overlap_end_expr} - {anchor_expr}) / 1e6, 3) AS end_ms, \
             p.name AS process_name, \
             p.upid AS upid, \
             p.pid AS pid, \
             t.name AS thread_name, \
             COALESCE( \
               MAX(CASE WHEN a.flat_key IN ( \
                 'debug.url', 'debug.data.url', 'debug.data.script_url', \
                 'debug.fileName', 'url', 'script_url', 'fileName' \
               ) THEN a.display_value END), \
               MAX(CASE WHEN a.key IN ('url', 'script_url', 'fileName') \
                    THEN a.display_value END), \
               MAX(CASE WHEN lower(a.flat_key) LIKE '%url%' \
                    THEN a.display_value END), \
               '<no-url>' \
             ) AS url \
           FROM slice s \
           JOIN thread_track tt ON s.track_id = tt.id \
           JOIN thread t ON tt.utid = t.utid \
           LEFT JOIN process p ON t.upid = p.upid \
           LEFT JOIN args a ON s.arg_set_id = a.arg_set_id "
    ));
    if page_window.phase.is_some() {
        sql.push_str("CROSS JOIN script_window sw ");
    }
    sql.push_str(
        "WHERE s.dur > 0 \
           AND (t.is_main_thread = 1 OR t.name GLOB 'Cr*Main') \
           AND ( \
             s.name IN ( \
               'EvaluateScript', 'v8.run', 'FunctionCall', \
               'v8.callFunction', 'RunMicrotasks' \
             ) OR \
             s.name GLOB '*ExecuteScript*' OR \
             s.name GLOB '*EvaluateScript*' OR \
             s.name GLOB '*ScriptRunner*' OR \
             s.name GLOB '*TimerFire*' \
           )",
    );
    if let Some(bound) = &start_bound {
        sql.push_str(&format!(" AND s.ts + s.dur > {bound}"));
    }
    if let Some(bound) = &end_bound {
        sql.push_str(&format!(" AND s.ts < {bound}"));
    }
    if page_window.phase.is_some() {
        sql.push_str(" AND sw.start_ts IS NOT NULL AND sw.end_ts IS NOT NULL");
    }
    if let (Some(start), Some(end)) = (&start_bound, &end_bound) {
        sql.push_str(&format!(" AND {end} > {start}"));
    }
    if let Some(name) = process_name {
        let lit = sql_string_literal(name)?;
        sql.push_str(&format!(" AND p.name = {lit}"));
    }
    if let Some(pid) = pid {
        sql.push_str(&format!(" AND p.pid = {pid}"));
    }
    if let Some(upid) = upid {
        sql.push_str(&format!(" AND p.upid = {upid}"));
    }
    sql.push_str(
        " GROUP BY \
             s.id, s.ts, s.dur, s.thread_dur, s.name, p.name, p.upid, p.pid, t.name \
         ), \
         script_descendants(root_id, id, depth) AS ( \
           SELECT id, id, 0 FROM script_slices \
           UNION ALL \
           SELECT sd.root_id, child.id, sd.depth + 1 \
           FROM script_descendants sd \
           JOIN slice child ON child.parent_id = sd.id \
           WHERE sd.depth < 8 AND child.dur > 0 \
         ), \
         descendant_rollup AS ( \
           SELECT \
             sd.root_id, \
             SUM(CASE WHEN \
               d.name GLOB '*Forced*Layout*' OR \
               d.name GLOB '*Forced*Style*' OR \
               d.name GLOB '*UpdateStyleAndLayout*' \
               THEN CASE WHEN d.ts + d.dur > root.overlap_start_ts \
                           AND d.ts < root.overlap_end_ts \
                         THEN MIN(d.ts + d.dur, root.overlap_end_ts) \
                              - MAX(d.ts, root.overlap_start_ts) \
                         ELSE 0 END \
               ELSE 0 END) AS forced_style_layout_ns, \
             SUM(CASE WHEN \
               d.name GLOB '*Recalculate*Style*' OR \
               d.name GLOB '*UpdateStyle*' OR \
               d.name GLOB '*StyleRecalc*' \
               THEN CASE WHEN d.ts + d.dur > root.overlap_start_ts \
                           AND d.ts < root.overlap_end_ts \
                         THEN MIN(d.ts + d.dur, root.overlap_end_ts) \
                              - MAX(d.ts, root.overlap_start_ts) \
                         ELSE 0 END \
               ELSE 0 END) AS style_recalc_ns, \
             SUM(CASE WHEN \
               d.name GLOB '*Layout*' OR \
               d.name GLOB '*UpdateLayout*' \
               THEN CASE WHEN d.ts + d.dur > root.overlap_start_ts \
                           AND d.ts < root.overlap_end_ts \
                         THEN MIN(d.ts + d.dur, root.overlap_end_ts) \
                              - MAX(d.ts, root.overlap_start_ts) \
                         ELSE 0 END \
               ELSE 0 END) AS layout_ns \
           FROM script_descendants sd \
           JOIN script_slices root ON root.id = sd.root_id \
           JOIN slice d ON d.id = sd.id \
           WHERE sd.depth > 0 \
           GROUP BY sd.root_id \
         ) \
         SELECT \
           ss.url, \
           ss.name, \
           ss.process_name, \
           ss.upid, \
           ss.pid, \
           ss.thread_name, \
           COUNT(*) AS slice_count, \
           ROUND(SUM(ss.overlap_dur) / 1e6, 3) AS total_wall_ms, \
           ROUND(MAX(ss.overlap_dur) / 1e6, 3) AS max_wall_ms, \
           ROUND(SUM(ss.overlap_thread_dur) / 1e6, 3) AS total_cpu_ms, \
           ROUND(SUM(COALESCE(dr.forced_style_layout_ns, 0)) / 1e6, 3) \
             AS forced_style_layout_ms, \
           ROUND(SUM(COALESCE(dr.style_recalc_ns, 0)) / 1e6, 3) AS style_recalc_ms, \
           ROUND(SUM(COALESCE(dr.layout_ns, 0)) / 1e6, 3) AS layout_ms, \
           MIN(ss.start_ms) AS first_start_ms, \
           MAX(ss.end_ms) AS last_end_ms, \
           (SELECT s2.id \
            FROM script_slices s2 \
            WHERE s2.url IS ss.url \
              AND s2.name IS ss.name \
              AND s2.upid IS ss.upid \
              AND s2.pid IS ss.pid \
              AND s2.thread_name IS ss.thread_name \
            ORDER BY s2.overlap_dur DESC, s2.dur DESC, s2.id ASC \
            LIMIT 1) AS example_slice_id \
         FROM script_slices ss \
         LEFT JOIN descendant_rollup dr ON dr.root_id = ss.id \
         GROUP BY ss.url, ss.name, ss.process_name, ss.upid, ss.pid, ss.thread_name ",
    );
    sql.push_str(&format!(
        "HAVING SUM(ss.overlap_dur) >= {min_total_ns} \
         ORDER BY total_wall_ms DESC, max_wall_ms DESC, first_start_ms ASC \
         LIMIT {row_limit}"
    ));
    Ok(sql)
}

/// SQL builder for `slice_descendants_breakdown`.
///
/// The query deliberately qualifies `d.depth` / `s.*` everywhere because the
/// `slice` table itself has a `depth` column; unqualified recursive CTEs are
/// a common source of `ambiguous column name: depth` errors in agent-written
/// follow-up analysis.
///
/// `example_slice_id` picks the longest-duration descendant in each
/// (root_id, depth, name) group (ties broken by smallest `slice.id`) so that
/// `include_args=true` surfaces args from the most diagnostically interesting
/// sample, not just the lowest-id one. `first_ts_ns` keeps the name in
/// nanoseconds because `slice.ts` is a wall-clock-ish nanosecond stamp; the
/// other duration columns are in ms, so the unit suffix makes the difference
/// visible to callers.
pub fn slice_descendants_breakdown_sql(
    filters: SliceDescendantsBreakdownFilters<'_>,
) -> Result<String, PerfettoError> {
    let SliceDescendantsBreakdownFilters {
        slice_ids,
        min_dur_ms,
        max_depth,
        include_args,
        row_limit,
    } = filters;

    if slice_ids.is_empty() {
        return Err(PerfettoError::InvalidParam(
            "slice_ids must contain at least one root slice id".to_owned(),
        ));
    }
    // Value-shape checks come before size checks so callers get the actionable
    // error (negative id) rather than a misleading "too many roots" when they
    // pass a long list with a single bad value.
    if let Some(id) = slice_ids.iter().find(|id| **id < 0) {
        return Err(PerfettoError::InvalidParam(format!(
            "slice ids must be non-negative, got {id}"
        )));
    }
    let deduped_slice_ids = dedupe_preserving_order(slice_ids);
    if deduped_slice_ids.len() > MAX_SLICE_DESCENDANTS_ROOTS {
        return Err(PerfettoError::InvalidParam(format!(
            "slice_ids accepts at most {MAX_SLICE_DESCENDANTS_ROOTS} roots, got {}",
            deduped_slice_ids.len()
        )));
    }
    if row_limit == 0 {
        return Err(PerfettoError::InvalidParam(
            "row_limit must be > 0; resolve via slice_descendants_effective_limit".to_owned(),
        ));
    }

    let min_dur_ms = min_dur_ms.unwrap_or(DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS);
    let min_dur_ns = {
        let ns = min_dur_ms * 1_000_000.0;
        if !(ns.is_finite() && ns >= 0.0 && ns <= i64::MAX as f64) {
            return Err(PerfettoError::InvalidParam(format!(
                "min_dur_ms must be finite, non-negative, and ≤ ~9.2e12 ms, got {min_dur_ms}"
            )));
        }
        ns as i64
    };

    let max_depth = match max_depth {
        None => DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
        Some(0) => {
            return Err(PerfettoError::InvalidParam(
                "max_depth must be > 0 when set".to_owned(),
            ));
        }
        Some(n) if n > 64 => {
            return Err(PerfettoError::InvalidParam(format!(
                "max_depth must be <= 64 to bound recursive expansion, got {n}"
            )));
        }
        Some(n) => n,
    };
    let roots = deduped_slice_ids
        .iter()
        .map(|id| format!("({id})"))
        .collect::<Vec<_>>()
        .join(", ");
    let args_column = if include_args {
        ", \
         (SELECT group_concat( \
             a.flat_key || '=' || COALESCE( \
               a.display_value, \
               CAST(a.int_value AS TEXT), \
               CAST(a.real_value AS TEXT), \
               a.string_value, \
               '' \
             ), \
             '; ' \
           ) \
          FROM slice ex \
          JOIN args a ON a.arg_set_id = ex.arg_set_id \
          WHERE ex.id = grouped.example_slice_id) AS example_args"
    } else {
        ""
    };

    // ROW_NUMBER() in `ranked` picks the longest-duration descendant per
    // (root_id, depth, name) group with `rn = 1`; ties break on smallest
    // slice.id so the choice is deterministic across re-runs. The aggregate
    // step in `grouped` then uses MAX(CASE WHEN rn=1 THEN id) to surface
    // that representative without colliding with the SUM/MAX(dur) aggregates.
    Ok(format!(
        "WITH RECURSIVE \
           roots(root_id) AS (VALUES {roots}), \
           descendants(root_id, slice_id, depth) AS ( \
             SELECT r.root_id, s.id AS slice_id, 0 AS depth \
             FROM roots r \
             JOIN slice s ON s.id = r.root_id \
             UNION ALL \
             SELECT d.root_id, child.id AS slice_id, d.depth + 1 AS depth \
             FROM descendants d \
             JOIN slice child ON child.parent_id = d.slice_id \
             WHERE d.depth < {max_depth} \
           ), \
           ranked AS ( \
             SELECT \
               d.root_id AS root_id, \
               d.depth AS depth, \
               s.name AS name, \
               s.id AS slice_id, \
               s.dur AS dur, \
               s.ts AS ts, \
               ROW_NUMBER() OVER ( \
                 PARTITION BY d.root_id, d.depth, s.name \
                 ORDER BY s.dur DESC, s.id ASC \
               ) AS rn \
             FROM descendants d \
             JOIN slice s ON s.id = d.slice_id \
             WHERE d.depth > 0 \
               AND s.dur >= {min_dur_ns} \
           ), \
           grouped AS ( \
             SELECT \
               root_id, \
               depth, \
               name, \
               COUNT(*) AS slice_count, \
               SUM(dur) / 1e6 AS total_ms, \
               MAX(dur) / 1e6 AS max_ms, \
               MIN(ts) AS first_ts_ns, \
               MAX(CASE WHEN rn = 1 THEN slice_id END) AS example_slice_id \
             FROM ranked \
             GROUP BY root_id, depth, name \
           ) \
         SELECT \
           grouped.root_id, \
           grouped.depth, \
           grouped.name, \
           grouped.slice_count, \
           ROUND(grouped.total_ms, 3) AS total_ms, \
           ROUND(grouped.max_ms, 3) AS max_ms, \
           grouped.first_ts_ns, \
           grouped.example_slice_id{args_column} \
         FROM grouped \
         ORDER BY grouped.total_ms DESC, grouped.max_ms DESC, \
                  grouped.slice_count DESC, grouped.root_id, grouped.depth, grouped.name \
         LIMIT {row_limit}"
    ))
}

/// Stable dedupe for slice id lists. Shared between the SQL builder (so the
/// recursive CTE seeds each root exactly once) and the handler (so the
/// pre-query that detects missing roots issues exactly one lookup per id).
pub fn dedupe_preserving_order(ids: &[i64]) -> Vec<i64> {
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

pub fn slice_descendants_effective_limit(limit: Option<u32>) -> Result<u32, PerfettoError> {
    match limit {
        None => Ok(DEFAULT_SLICE_DESCENDANTS_LIMIT),
        Some(0) => Err(PerfettoError::InvalidParam(
            "limit must be > 0 when set".to_owned(),
        )),
        Some(n) if (n as usize) > MAX_ROWS => Ok(MAX_ROWS as u32),
        Some(n) => Ok(n),
    }
}

/// SQL for chrome_web_content_interactions. Exported for integration tests.
pub const CHROME_WEB_CONTENT_INTERACTIONS_SQL: &str =
    "INCLUDE PERFETTO MODULE chrome.web_content_interactions; \
     SELECT \
       id, \
       ts, \
       dur / 1e6 AS dur_ms, \
       interaction_type, \
       renderer_upid \
     FROM chrome_web_content_interactions \
     ORDER BY dur DESC \
     LIMIT 100";

/// SQL for chrome_startup_summary. Exported for integration tests.
pub const CHROME_STARTUP_SUMMARY_SQL: &str = "INCLUDE PERFETTO MODULE chrome.startups; \
     SELECT \
       id, \
       name, \
       launch_cause, \
       (first_visible_content_ts - startup_begin_ts) / 1e6 AS startup_duration_ms, \
       startup_begin_ts, \
       first_visible_content_ts, \
       browser_upid \
     FROM chrome_startups \
     ORDER BY startup_begin_ts DESC \
     LIMIT 100";

/// Preflight SQL for chrome_* tools — checks for the `chrome.process_type`
/// track-descriptor arg that Chromium emits for every Chrome-family
/// process. Chosen over process-name matching (`'Browser'`/`'Renderer'`/
/// `'GPU Process'`) because those aliases are desktop-specific and miss
/// variants such as Chrome for Android (`com.android.chrome:…` process
/// names), WebView, Chromium, and Electron. Returns 1 if the arg is
/// present on any track, 0 otherwise.
///
/// Coverage note: verified against the bundled `scroll_jank.pftrace` and
/// `page_loads.pftrace` (desktop Chrome) and `basic.perfetto-trace`
/// (non-Chrome). Android/WebView/Chromium/Electron coverage is inferred
/// from Perfetto stdlib's own use of `chrome.process_type` but not
/// independently verified here — treat as a best-effort gate with the
/// `execute_sql` escape hatch available for any false negative.
///
/// Exported for integration tests.
pub const CHROME_TRACE_PREFLIGHT_SQL: &str =
    "SELECT EXISTS(SELECT 1 FROM args WHERE flat_key = 'chrome.process_type') AS n";

/// Validate a glob parameter — only allows alphanumeric, `.`, `_`, `-`, `:`, `*`, `?`.
pub fn sanitize_glob_param(s: &str) -> Result<String, PerfettoError> {
    if !s
        .chars()
        .all(|c| c.is_alphanumeric() || "._-:*?".contains(c))
    {
        return Err(PerfettoError::InvalidParam(format!(
            "Invalid parameter: {s:?}"
        )));
    }
    Ok(s.to_owned())
}

/// Escape a user-supplied string for inclusion in a SQL single-quoted literal.
///
/// Doubles single quotes (the SQL-standard escape) and rejects any control
/// character. Used for fields like process names that contain spaces, dots,
/// or slashes — where `sanitize_glob_param`'s strict charset would reject
/// valid input. The returned value includes the surrounding quotes.
pub fn sql_string_literal(s: &str) -> Result<String, PerfettoError> {
    if s.chars().any(|c| c.is_control()) {
        return Err(PerfettoError::InvalidParam(format!(
            "Invalid parameter (contains control character): {s:?}"
        )));
    }
    Ok(format!("'{}'", s.replace('\'', "''")))
}

#[cfg(test)]
mod tests {
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
}
