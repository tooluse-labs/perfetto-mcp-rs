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
        machine_id,
        process_machine_id_available,
        upid,
        page_load_id,
        navigation_id,
        phase,
        start_ts_ns,
        end_ts_ns,
        min_dur_ms,
        limit,
    } = filters;
    if machine_id.is_some() && !process_machine_id_available {
        return Err(PerfettoError::InvalidParam(
            "machine_id filter requires a trace schema with process.machine_id".to_owned(),
        ));
    }
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
    let machine_id_expr = if process_machine_id_available {
        "p.machine_id"
    } else {
        "NULL"
    };
    let effective_phase = page_window.phase;
    let start_bound = match (effective_phase.is_some(), page_window.start_ts_ns) {
        (true, Some(ts)) => Some(format!("MAX(hw.start_ts, {ts})")),
        (true, None) => Some("hw.start_ts".to_owned()),
        (false, Some(ts)) => Some(ts.to_string()),
        (false, None) => None,
    };
    let end_bound = match (effective_phase.is_some(), page_window.end_ts_ns) {
        (true, Some(ts)) => Some(format!("MIN(hw.end_ts, {ts})")),
        (true, None) => Some("hw.end_ts".to_owned()),
        (false, Some(ts)) => Some(ts.to_string()),
        (false, None) => None,
    };
    let overlap_start_expr = match &start_bound {
        Some(bound) => format!("MAX(ct.ts, {bound})"),
        None => "ct.ts".to_owned(),
    };
    let overlap_end_expr = match &end_bound {
        Some(bound) => format!("MIN(ct.ts + ct.dur, {bound})"),
        None => "ct.ts + ct.dur".to_owned(),
    };
    let overlap_dur_expr = match (&start_bound, &end_bound) {
        (None, None) => "ct.dur".to_owned(),
        _ => format!("({overlap_end_expr} - {overlap_start_expr})"),
    };
    let overlap_thread_dur_expr = format!(
        "MAX(MIN( \
          ct.thread_dur * {overlap_dur_expr} * 1.0 / ct.dur, \
          {overlap_dur_expr} \
        ), 0.0)"
    );
    let order_expr = match (&start_bound, &end_bound) {
        (None, None) => "ct.dur DESC".to_owned(),
        _ => format!("{overlap_dur_expr} DESC, ct.dur DESC"),
    };
    let mut sql = String::from("INCLUDE PERFETTO MODULE chrome.tasks; ");
    append_chrome_page_load_window_cte(
        &mut sql,
        "hotspot_window",
        page_window_filters,
        page_window,
    );
    sql.push_str(&format!(
        "SELECT \
           ct.id, \
           ct.ts, \
           ct.name, \
           ct.task_type, \
           ct.thread_name, \
           ct.process_name, \
           ct.upid, \
           p.pid, \
           {machine_id_expr} AS machine_id, \
           ct.dur / 1e6 AS dur_ms, \
           ROUND({overlap_dur_expr} / 1e6, 3) AS overlap_dur_ms, \
           CASE WHEN ct.thread_dur IS NOT NULL AND ct.dur > 0 \
                THEN MAX(MIN(ROUND(ct.thread_dur * 100.0 / ct.dur, 1), 100.0), 0.0) \
           END AS cpu_pct, \
           CASE WHEN ct.thread_dur IS NOT NULL AND ct.dur > 0 AND {overlap_dur_expr} > 0 \
                THEN ROUND({overlap_thread_dur_expr} * 100.0 / {overlap_dur_expr}, 1) \
           END AS overlap_cpu_pct, \
           ct.thread_dur / 1e6 AS thread_dur_ms, \
           CASE WHEN ct.thread_dur IS NOT NULL AND ct.dur > 0 \
                THEN ROUND({overlap_thread_dur_expr} / 1e6, 3) \
           END AS overlap_thread_dur_ms \
         FROM chrome_tasks ct \
         LEFT JOIN thread t ON ct.utid = t.utid \
         LEFT JOIN process p ON ct.upid = p.upid ",
    ));
    if effective_phase.is_some() {
        sql.push_str("CROSS JOIN hotspot_window hw ");
    }
    sql.push_str(&format!(
        "WHERE (t.is_main_thread = 1 OR ct.thread_name GLOB 'Cr*Main') \
           AND {overlap_dur_expr} >= {min_dur_ns}"
    ));
    if effective_phase.is_some() {
        sql.push_str(
            " AND hw.start_ts IS NOT NULL \
              AND hw.end_ts IS NOT NULL",
        );
    }
    if let Some(bound) = &start_bound {
        sql.push_str(&format!(" AND ct.ts + ct.dur > {bound}"));
    }
    if let Some(bound) = &end_bound {
        sql.push_str(&format!(" AND ct.ts < {bound}"));
    }
    if let (Some(start), Some(end)) = (&start_bound, &end_bound) {
        sql.push_str(&format!(" AND {end} > {start}"));
    }
    if let Some(name) = process_name {
        let lit = sql_string_literal(name)?;
        sql.push_str(&format!(" AND ct.process_name = {lit}"));
    }
    if let Some(pid) = pid {
        sql.push_str(&format!(" AND p.pid = {pid}"));
    }
    if let Some(machine_id) = machine_id {
        sql.push_str(&format!(" AND p.machine_id = {machine_id}"));
    }
    if let Some(upid) = upid {
        sql.push_str(&format!(" AND p.upid = {upid}"));
    }
    sql.push_str(&format!(" ORDER BY {order_expr} LIMIT {row_limit}"));
    Ok(sql)
}
