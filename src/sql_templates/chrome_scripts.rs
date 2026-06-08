use crate::error::PerfettoError;
use crate::params::ChromePageLoadScriptHotspotsFilters;

use super::chrome_common::{
    append_chrome_page_load_window_cte_body, chrome_tool_row_limit, duration_ms_to_ns,
    validate_chrome_page_load_window,
};
use super::sanitize::sql_string_literal;
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
        machine_id,
        process_machine_id_available,
        upid,
        window,
        min_total_ms,
        limit,
    } = filters;
    if machine_id.is_some() && !process_machine_id_available {
        return Err(PerfettoError::InvalidParam(
            "machine_id filter requires a trace schema with process.machine_id".to_owned(),
        ));
    }
    let page_window = validate_chrome_page_load_window(window)?;
    let min_total_ns = duration_ms_to_ns("min_total_ms", min_total_ms, 20_000_000)?;
    let row_limit = chrome_tool_row_limit(limit)?;
    let machine_id_expr = if process_machine_id_available {
        "p.machine_id"
    } else {
        "NULL"
    };

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
                  THEN MAX(MIN( \
                    s.thread_dur * {overlap_dur_expr} * 1.0 / s.dur, \
                    {overlap_dur_expr} \
                  ), 0.0) \
             END AS overlap_thread_dur, \
             s.name, \
             ROUND(({overlap_start_expr} - {anchor_expr}) / 1e6, 3) AS start_ms, \
             ROUND(({overlap_end_expr} - {anchor_expr}) / 1e6, 3) AS end_ms, \
             p.name AS process_name, \
             p.upid AS upid, \
             p.pid AS pid, \
             {machine_id_expr} AS machine_id, \
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
    if let Some(machine_id) = machine_id {
        sql.push_str(&format!(" AND p.machine_id = {machine_id}"));
    }
    if let Some(upid) = upid {
        sql.push_str(&format!(" AND p.upid = {upid}"));
    }
    sql.push_str(&format!(
        " GROUP BY \
             s.id, s.ts, s.dur, s.thread_dur, s.name, \
             p.name, p.upid, p.pid, {machine_id_expr}, t.name \
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
             SUM(CASE WHEN ( \
               d.name GLOB '*Recalculate*Style*' OR \
               d.name GLOB '*UpdateStyle*' OR \
               d.name GLOB '*StyleRecalc*' \
             ) \
               AND NOT ( \
                 d.name GLOB '*Forced*Layout*' OR \
                 d.name GLOB '*Forced*Style*' OR \
                 d.name GLOB '*UpdateStyleAndLayout*' \
               ) \
               THEN CASE WHEN d.ts + d.dur > root.overlap_start_ts \
                           AND d.ts < root.overlap_end_ts \
                         THEN MIN(d.ts + d.dur, root.overlap_end_ts) \
                              - MAX(d.ts, root.overlap_start_ts) \
                         ELSE 0 END \
               ELSE 0 END) AS style_recalc_ns, \
             SUM(CASE WHEN ( \
               d.name GLOB '*Layout*' OR \
               d.name GLOB '*UpdateLayout*' \
             ) \
               AND NOT ( \
                 d.name GLOB '*Forced*Layout*' OR \
                 d.name GLOB '*Forced*Style*' OR \
                 d.name GLOB '*UpdateStyleAndLayout*' \
               ) \
               AND NOT ( \
                 d.name GLOB '*Recalculate*Style*' OR \
                 d.name GLOB '*UpdateStyle*' OR \
                 d.name GLOB '*StyleRecalc*' \
               ) \
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
           ss.machine_id, \
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
              AND s2.machine_id IS ss.machine_id \
              AND s2.thread_name IS ss.thread_name \
            ORDER BY s2.overlap_dur DESC, s2.dur DESC, s2.id ASC \
            LIMIT 1) AS example_slice_id \
         FROM script_slices ss \
         LEFT JOIN descendant_rollup dr ON dr.root_id = ss.id \
         GROUP BY ss.url, ss.name, ss.process_name, ss.upid, ss.pid, \
                  ss.machine_id, ss.thread_name ",
    ));
    sql.push_str(&format!(
        "HAVING SUM(ss.overlap_dur) >= {min_total_ns} \
         ORDER BY total_wall_ms DESC, max_wall_ms DESC, first_start_ms ASC \
         LIMIT {row_limit}"
    ));
    Ok(sql)
}
