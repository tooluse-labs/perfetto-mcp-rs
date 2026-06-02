use crate::error::PerfettoError;
use crate::params::{ChromeMainThreadHotspotsFilters, ChromePageLoadWindowFilters};

use super::chrome_common::{
    append_chrome_page_load_window_cte, chrome_tool_row_limit, duration_ms_to_ns,
    validate_chrome_page_load_window,
};
use super::sanitize::sql_string_literal;
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
