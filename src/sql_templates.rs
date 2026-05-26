// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use crate::error::{PerfettoError, MAX_ROWS};
use crate::params::{
    ChromeMainThreadHotspotsFilters, ChromeMainThreadHotspotsPhase,
    SliceDescendantsBreakdownFilters,
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
       url, \
       navigation_start_ts, \
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
    let min_dur_ns: i64 = match min_dur_ms {
        None => 16_000_000,
        Some(ms) => {
            let ns = ms * 1_000_000.0;
            if !(ns.is_finite() && ns >= 0.0 && ns <= i64::MAX as f64) {
                return Err(PerfettoError::InvalidParam(format!(
                    "min_dur_ms must be finite, non-negative, and ≤ ~9.2e12 ms, got {ms}"
                )));
            }
            ns as i64
        }
    };
    let row_limit: u32 = match limit {
        None => 100,
        Some(0) => {
            return Err(PerfettoError::InvalidParam("limit must be > 0".to_owned()));
        }
        Some(n) if (n as usize) > MAX_ROWS => MAX_ROWS as u32,
        Some(n) => n,
    };
    let effective_phase = phase.or_else(|| {
        (page_load_id.is_some() || navigation_id.is_some())
            .then_some(ChromeMainThreadHotspotsPhase::NavigationToFcp)
    });
    let mut sql = String::from("INCLUDE PERFETTO MODULE chrome.tasks; ");
    if let Some(phase) = effective_phase {
        let (start_expr, end_expr) = match phase {
            ChromeMainThreadHotspotsPhase::NavigationToFcp => ("navigation_start_ts", "fcp_ts"),
            ChromeMainThreadHotspotsPhase::NavigationToLoad => {
                ("navigation_start_ts", "load_event_ts")
            }
            ChromeMainThreadHotspotsPhase::DclToFcp => ("dom_content_loaded_event_ts", "fcp_ts"),
            ChromeMainThreadHotspotsPhase::FcpToLoad => ("fcp_ts", "load_event_ts"),
        };
        sql.push_str("INCLUDE PERFETTO MODULE chrome.page_loads; ");
        sql.push_str(&format!(
            "WITH hotspot_window AS ( \
             SELECT {start_expr} AS start_ts, {end_expr} AS end_ts \
             FROM chrome_page_loads "
        ));
        if let Some(id) = page_load_id {
            sql.push_str(&format!("WHERE id = {id} "));
        }
        if let Some(id) = navigation_id {
            sql.push_str(&format!("WHERE navigation_id = {id} "));
        }
        sql.push_str("ORDER BY navigation_start_ts DESC LIMIT 1) ");
    }
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
    if let Some(start) = start_ts_ns {
        sql.push_str(&format!(" AND ct.ts >= {start}"));
    }
    if let Some(end) = end_ts_ns {
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
