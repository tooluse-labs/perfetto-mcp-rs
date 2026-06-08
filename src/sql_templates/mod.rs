// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

mod chrome_common;
mod chrome_main_thread;
mod chrome_resources;
mod chrome_scripts;
mod sanitize;
mod slice_descendants;

pub use chrome_main_thread::chrome_main_thread_hotspots_sql;
pub use chrome_resources::{
    chrome_page_load_resource_hotspots_sql, chrome_page_load_resource_pipeline_sql,
    chrome_page_load_resource_summary_sql, chrome_page_load_resource_timing_evidence_sql,
};
pub use chrome_scripts::chrome_page_load_script_hotspots_sql;
pub use sanitize::{sanitize_glob_param, sql_string_literal};
pub use slice_descendants::{
    dedupe_preserving_order, slice_descendants_breakdown_sql, slice_descendants_effective_limit,
    DEFAULT_SLICE_DESCENDANTS_LIMIT, DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
    DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS, MAX_SLICE_DESCENDANTS_ROOTS,
};

#[cfg(test)]
use crate::params::SliceDescendantsBreakdownFilters;
#[cfg(test)]
use chrome_resources::{chrome_resource_url_origin_expr, chrome_url_arg_priority_expr};

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

pub const CHROME_SCROLL_JANK_SUMMARY_COUNT_SQL: &str =
    "INCLUDE PERFETTO MODULE chrome.scroll_jank.scroll_jank_v3; \
     SELECT COUNT(*) AS row_count FROM chrome_janky_frames";

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

pub const CHROME_PAGE_LOAD_SUMMARY_COUNT_SQL: &str = "INCLUDE PERFETTO MODULE chrome.page_loads; \
     SELECT COUNT(*) AS row_count FROM chrome_page_loads";

/// SQL for chrome_web_content_interactions. Exported for integration tests.
pub const CHROME_WEB_CONTENT_INTERACTIONS_SQL: &str =
    "INCLUDE PERFETTO MODULE chrome.web_content_interactions; \
     SELECT \
       id, \
       ts, \
       total_duration_ms, \
       dur / 1e6 AS longest_event_dur_ms, \
       interaction_type, \
       renderer_upid \
     FROM chrome_web_content_interactions \
     ORDER BY total_duration_ms DESC, dur DESC \
     LIMIT 100";

pub const CHROME_WEB_CONTENT_INTERACTIONS_COUNT_SQL: &str =
    "INCLUDE PERFETTO MODULE chrome.web_content_interactions; \
     SELECT COUNT(*) AS row_count FROM chrome_web_content_interactions";

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

pub const CHROME_STARTUP_SUMMARY_COUNT_SQL: &str = "INCLUDE PERFETTO MODULE chrome.startups; \
     SELECT COUNT(*) AS row_count FROM chrome_startups";

pub fn chrome_scroll_jank_summary_sql(row_limit: usize) -> String {
    CHROME_SCROLL_JANK_SUMMARY_SQL.replace("LIMIT 100", &format!("LIMIT {row_limit}"))
}

pub fn chrome_page_load_summary_sql(row_limit: usize) -> String {
    CHROME_PAGE_LOAD_SUMMARY_SQL.replace("LIMIT 100", &format!("LIMIT {row_limit}"))
}

pub fn chrome_web_content_interactions_sql(row_limit: usize) -> String {
    CHROME_WEB_CONTENT_INTERACTIONS_SQL.replace("LIMIT 100", &format!("LIMIT {row_limit}"))
}

pub fn chrome_startup_summary_sql(row_limit: usize) -> String {
    CHROME_STARTUP_SUMMARY_SQL.replace("LIMIT 100", &format!("LIMIT {row_limit}"))
}

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

#[cfg(test)]
mod tests;
