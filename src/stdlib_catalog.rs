// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

/// Server-level `instructions` shipped on MCP handshake. Lists curated
/// PerfettoSQL stdlib modules so agents stop hand-rolling `LIKE '%x%'` scans
/// on the raw `slice` table. Module names and their exposed public
/// tables/views are taken from the vendored Perfetto stdlib source.
///
/// The same stdlib guidance is also carried on the `execute_sql` tool
/// description (the `tools/list` channel v0.3/0.4 samples confirmed reaches
/// Claude Code agents). This multi-channel redundancy is by design:
/// instructions token cost is paid once at handshake, so future agent
/// frameworks or MCP clients that do route `instructions` into the system
/// prompt get the nudge for free.
pub const STDLIB_INSTRUCTIONS: &str = "Perfetto trace analysis server. \
    Start by calling load_trace with a path to a Perfetto trace file (.pftrace, \
    .perfetto-trace, .bin, or any other trace_processor-readable format), \
    then use list_tables and list_table_structure to discover the schema, and \
    execute_sql to query.\n\
    \n\
    PREFER PerfettoSQL stdlib over raw `slice` + `LIKE '%x%'` scans. Call \
    `INCLUDE PERFETTO MODULE <name>` then query the exposed table/view \
    (INCLUDE and SELECT can be in a single execute_sql call):\n\
    \n\
    Chrome traces:\n\
    - chrome.page_loads -> chrome_page_loads (navigations, FCP, LCP, DCL)\n\
    - chrome.scroll_jank.scroll_jank_v3 -> chrome_janky_frames (scroll jank causes)\n\
    - chrome.tasks -> chrome_tasks (renderer/browser main-thread tasks)\n\
    - chrome.startups -> chrome_startups (browser process startup)\n\
    - chrome.web_content_interactions -> chrome_web_content_interactions (input latency, INP)\n\
    \n\
    Android traces:\n\
    - android.startup.startups -> android_startups (app cold/warm start)\n\
    - android.anrs -> android_anrs (ANR detection)\n\
    - android.binder -> android_binder_txns (binder IPC)\n\
    \n\
    Generic (any trace):\n\
    - slices.with_context -> thread_slice, process_slice (use INSTEAD OF manual \
      thread_track -> thread -> process JOIN chain)\n\
    - linux.cpu.frequency -> cpu_frequency_counters (CPU frequency)\n\
    \n\
    For modules not listed here (memory.*, wattson.*, sched.*, android.frames.*, \
    etc.), fetch https://perfetto.dev/docs/analysis/stdlib-docs before falling \
    back to raw slice scans.";

/// Curated PerfettoSQL stdlib modules as a JSON array. Targets the default
/// downloaded trace_processor_shell version. If PERFETTO_TP_PATH points to
/// a custom binary, some modules may not be available — use list_table_structure
/// after INCLUDE to confirm. Exported for integration tests.
pub const STDLIB_MODULE_LIST: &str = r#"[
  {
    "domain": "chrome",
    "module": "chrome.page_loads",
    "views": ["chrome_page_loads"],
    "description": "Chrome navigations with FCP, LCP, DCL, and load timing in ms",
    "usage": "INCLUDE PERFETTO MODULE chrome.page_loads; SELECT id, url, fcp / 1e6 AS fcp_ms, lcp / 1e6 AS lcp_ms FROM chrome_page_loads ORDER BY navigation_start_ts"
  },
  {
    "domain": "chrome",
    "module": "chrome.scroll_jank.scroll_jank_v3",
    "views": ["chrome_janky_frames"],
    "description": "Scroll jank cause distribution - cause_of_jank, sub_cause_of_jank, delay_since_last_frame",
    "usage": "INCLUDE PERFETTO MODULE chrome.scroll_jank.scroll_jank_v3; SELECT cause_of_jank, sub_cause_of_jank, COUNT(*) AS n FROM chrome_janky_frames GROUP BY cause_of_jank, sub_cause_of_jank ORDER BY n DESC"
  },
  {
    "domain": "chrome",
    "module": "chrome.tasks",
    "views": ["chrome_tasks"],
    "description": "Chrome main-thread and background task durations (id, name, task_type, thread_name, process_name, dur, thread_dur)",
    "usage": "INCLUDE PERFETTO MODULE chrome.tasks; SELECT name, task_type, thread_name, dur / 1e6 AS dur_ms FROM chrome_tasks WHERE thread_name IN ('CrBrowserMain','CrRendererMain') AND dur > 16000000 ORDER BY dur DESC LIMIT 50"
  },
  {
    "domain": "chrome",
    "module": "chrome.startups",
    "views": ["chrome_startups"],
    "description": "Chrome browser startup events - name, launch_cause, startup_duration (first_visible_content_ts - startup_begin_ts)",
    "usage": "INCLUDE PERFETTO MODULE chrome.startups; SELECT id, name, launch_cause, (first_visible_content_ts - startup_begin_ts) / 1e6 AS startup_ms FROM chrome_startups ORDER BY startup_begin_ts"
  },
  {
    "domain": "chrome",
    "module": "chrome.web_content_interactions",
    "views": ["chrome_web_content_interactions"],
    "description": "Input latency and Interaction to Next Paint (INP) in Chrome traces",
    "usage": "INCLUDE PERFETTO MODULE chrome.web_content_interactions; SELECT * FROM chrome_web_content_interactions LIMIT 20"
  },
  {
    "domain": "android",
    "module": "android.startup.startups",
    "views": ["android_startups"],
    "description": "Android app cold/warm startup phases and total launch duration",
    "usage": "INCLUDE PERFETTO MODULE android.startup.startups; SELECT * FROM android_startups LIMIT 20"
  },
  {
    "domain": "android",
    "module": "android.anrs",
    "views": ["android_anrs"],
    "description": "Android ANR (Application Not Responding) detection",
    "usage": "INCLUDE PERFETTO MODULE android.anrs; SELECT * FROM android_anrs LIMIT 20"
  },
  {
    "domain": "android",
    "module": "android.binder",
    "views": ["android_binder_txns"],
    "description": "Android Binder IPC transactions with caller/callee and duration",
    "usage": "INCLUDE PERFETTO MODULE android.binder; SELECT * FROM android_binder_txns LIMIT 50"
  },
  {
    "domain": "generic",
    "module": "slices.with_context",
    "views": ["thread_slice", "process_slice"],
    "description": "Slice with thread and process names pre-joined - use this INSTEAD OF the manual slice->thread_track->thread->process JOIN chain",
    "usage": "INCLUDE PERFETTO MODULE slices.with_context; SELECT name, thread_name, process_name, dur / 1e6 AS dur_ms FROM thread_slice WHERE dur > 10000000 ORDER BY dur DESC LIMIT 50"
  },
  {
    "domain": "generic",
    "module": "linux.cpu.frequency",
    "views": ["cpu_frequency_counters"],
    "description": "CPU frequency over time per core",
    "usage": "INCLUDE PERFETTO MODULE linux.cpu.frequency; SELECT * FROM cpu_frequency_counters LIMIT 50"
  }
]"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdlib_module_list_and_instructions_are_in_sync() {
        let list: Vec<serde_json::Value> =
            serde_json::from_str(STDLIB_MODULE_LIST).expect("STDLIB_MODULE_LIST is valid JSON");
        for entry in &list {
            let module = entry["module"].as_str().unwrap();
            assert!(
                STDLIB_INSTRUCTIONS.contains(module),
                "STDLIB_INSTRUCTIONS is missing module `{module}` that STDLIB_MODULE_LIST lists — \
                 update STDLIB_INSTRUCTIONS or remove the module from the list",
            );
            for view in entry["views"].as_array().unwrap() {
                let view = view.as_str().unwrap();
                assert!(
                    STDLIB_INSTRUCTIONS.contains(view),
                    "STDLIB_INSTRUCTIONS is missing view `{view}` for module `{module}`",
                );
            }
        }
    }
}
