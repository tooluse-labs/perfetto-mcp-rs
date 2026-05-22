// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

use crate::error::{PerfettoError, MAX_ROWS};
use crate::params::ChromeMainThreadHotspotsFilters;

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
        min_dur_ms,
        limit,
    } = filters;
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
    let mut sql = format!(
        "INCLUDE PERFETTO MODULE chrome.tasks; \
         SELECT \
           ct.id, \
           ct.ts, \
           ct.name, \
           ct.task_type, \
           ct.thread_name, \
           ct.process_name, \
           ct.dur / 1e6 AS dur_ms, \
           CASE WHEN ct.thread_dur IS NOT NULL AND ct.dur > 0 \
                THEN ROUND(ct.thread_dur * 100.0 / ct.dur, 1) \
           END AS cpu_pct, \
           ct.thread_dur / 1e6 AS thread_dur_ms \
         FROM chrome_tasks ct \
         LEFT JOIN thread t ON ct.utid = t.utid \
         LEFT JOIN process p ON ct.upid = p.upid \
         WHERE (t.is_main_thread = 1 OR ct.thread_name GLOB 'Cr*Main') \
           AND ct.dur > {min_dur_ns}",
    );
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
}
