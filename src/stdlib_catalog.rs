// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

/// Server-level `instructions` shipped on MCP handshake. Keep this short:
/// many MCP clients inject it into every LLM session. Longer stdlib guidance
/// lives in `STDLIB_QUICKREF`, exposed as a resource and mirrored by the
/// structured `list_stdlib_modules` tool for tools-only clients.
pub const STDLIB_INSTRUCTIONS: &str = "Call load_trace first. Use dedicated \
    chrome_* tools for common Chrome questions. Use list_tables and \
    list_table_structure for schema discovery, and execute_sql for custom \
    PerfettoSQL. Prefer PerfettoSQL stdlib modules over raw slice LIKE scans; \
    when unsure, read resource://perfetto-mcp/stdlib-quickref or call \
    list_stdlib_modules.";

pub const STDLIB_QUICKREF_URI: &str = "resource://perfetto-mcp/stdlib-quickref";
pub const STDLIB_QUICKREF_MIME_TYPE: &str = "text/markdown";

/// Human-readable stdlib guidance kept out of the default handshake. This is
/// intentionally concise but still teaches the routing pattern and the most
/// useful module names.
pub const STDLIB_QUICKREF: &str = r#"# PerfettoSQL stdlib quick reference

Use `INCLUDE PERFETTO MODULE <module>;` before querying a stdlib view. The
`INCLUDE` and `SELECT` can be sent in one `execute_sql` call.

Prefer stdlib views over raw `slice` scans when a module fits the question.
Use `list_stdlib_modules` for structured filtering by domain or keyword, and
`list_table_structure` after an `INCLUDE` if a column name is uncertain.

## Chrome

- `chrome.page_loads` -> `chrome_page_loads`: navigations, FCP, LCP, DCL, load.
- `chrome.scroll_jank.scroll_jank_v3` -> `chrome_janky_frames`: scroll jank causes.
- `chrome.tasks` -> `chrome_tasks`: browser/renderer tasks with process and thread context.
- `chrome.startups` -> `chrome_startups`: browser startup events.
- `chrome.web_content_interactions` -> `chrome_web_content_interactions`: input latency and INP.

## Android

- `android.startup.startups` -> `android_startups`: cold/warm app startup.
- `android.anrs` -> `android_anrs`: ANR detection.
- `android.binder` -> `android_binder_txns`: Binder IPC transactions.

## Generic

- `slices.with_context` -> `thread_slice`, `process_slice`: slice rows pre-joined
  with thread/process context.
- `linux.cpu.frequency` -> `cpu_frequency_counters`: CPU frequency counters.

For modules outside this curated list, use the Perfetto stdlib docs:
https://perfetto.dev/docs/analysis/stdlib-docs
"#;

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
    fn stdlib_module_list_and_quickref_are_in_sync() {
        let list: Vec<serde_json::Value> =
            serde_json::from_str(STDLIB_MODULE_LIST).expect("STDLIB_MODULE_LIST is valid JSON");
        for entry in &list {
            let module = entry["module"].as_str().unwrap();
            assert!(
                STDLIB_QUICKREF.contains(module),
                "STDLIB_QUICKREF is missing module `{module}` that STDLIB_MODULE_LIST lists — \
                 update STDLIB_QUICKREF or remove the module from the list",
            );
            for view in entry["views"].as_array().unwrap() {
                let view = view.as_str().unwrap();
                assert!(
                    STDLIB_QUICKREF.contains(view),
                    "STDLIB_QUICKREF is missing view `{view}` for module `{module}`",
                );
            }
        }
    }

    #[test]
    fn stdlib_instructions_stay_routing_sized() {
        assert!(
            STDLIB_INSTRUCTIONS.len() <= 450,
            "STDLIB_INSTRUCTIONS should stay short routing text, got {} chars",
            STDLIB_INSTRUCTIONS.len(),
        );
    }
}
