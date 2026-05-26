// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

//! e2e coverage for the five Chrome domain tools. Each test drives the
//! exact SQL the tool ships against a real fixture, so a future edit to the
//! stdlib view schema or the SQL constant surfaces as a test failure.
//!
//! Fixture applicability (verified against trace_processor_shell v54.0):
//! - scroll_jank.pftrace: chrome_janky_frames (6 rows) — strong e2e for
//!   scroll_jank_summary.
//! - page_loads.pftrace: chrome_page_loads (8 rows) — strong e2e for
//!   page_load_summary. It has main-thread tasks available via
//!   `is_main_thread` and/or Chrome `Cr*Main` naming, but zero verified tasks
//!   exceed the 16 ms threshold the tool filters by, so main_thread_hotspots
//!   falls back to a weak assertion.
//! - Neither fixture has chrome_startups or chrome_web_content_interactions
//!   data, so those two tools also use weak assertions. Upgrade to strong
//!   assertions when fixtures with the relevant event types are added.

use std::path::Path;

use perfetto_mcp_rs::params::ChromeMainThreadHotspotsFilters;
use perfetto_mcp_rs::sql_templates::{
    chrome_main_thread_hotspots_sql, CHROME_PAGE_LOAD_SUMMARY_SQL, CHROME_SCROLL_JANK_SUMMARY_SQL,
    CHROME_STARTUP_SUMMARY_SQL, CHROME_TRACE_PREFLIGHT_SQL, CHROME_WEB_CONTENT_INTERACTIONS_SQL,
};
use perfetto_mcp_rs::tp_manager::TraceProcessorManager;

#[test]
fn e2e_chrome_scroll_jank_summary_against_fixture() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_101);
        let trace = Path::new("tests/fixtures/scroll_jank.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let table = client
            .query(CHROME_SCROLL_JANK_SUMMARY_SQL)
            .await
            .expect("chrome scroll jank query must succeed on scroll_jank.pftrace");

        assert!(
            !table.is_empty(),
            "scroll_jank.pftrace must yield at least one chrome_janky_frames row",
        );
        for i in 0..table.len() {
            assert!(
                table.cell(i, "cause_of_jank").is_some(),
                "row {i} missing cause_of_jank column",
            );
            assert!(
                table.cell(i, "delay_since_last_frame").is_some(),
                "row {i} missing delay_since_last_frame column",
            );
            assert!(
                table.cell(i, "event_latency_id").is_some(),
                "row {i} missing event_latency_id column",
            );
        }
    });
}

#[test]
fn e2e_chrome_page_load_summary_against_fixture() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_201);
        let trace = Path::new("tests/fixtures/page_loads.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let table = client
            .query(CHROME_PAGE_LOAD_SUMMARY_SQL)
            .await
            .expect("chrome page load query must succeed on page_loads.pftrace");

        assert!(
            !table.is_empty(),
            "page_loads.pftrace must yield at least one chrome_page_loads row",
        );
        for i in 0..table.len() {
            assert!(table.cell(i, "id").is_some(), "row {i} missing id column",);
            assert!(table.cell(i, "url").is_some(), "row {i} missing url column",);
            assert!(
                table.cell(i, "navigation_start_ts").is_some(),
                "row {i} missing navigation_start_ts column",
            );
        }
    });
}

#[test]
fn e2e_chrome_main_thread_hotspots_against_fixture() {
    // Weak assertion: SQL executes cleanly. page_loads.pftrace has
    // main-thread tasks, but verified 0 of them exceed the 16 ms threshold
    // (all tasks well under frame budget on that capture), so empty rows is
    // a valid passing state here. scroll_jank.pftrace is not usable — it has
    // 0 chrome_tasks rows total. Upgrade to a strong assertion when a fixture
    // with main-thread tasks > 16 ms becomes available.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_301);
        let trace = Path::new("tests/fixtures/page_loads.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters::default())
            .expect("hotspots SQL builder must succeed");
        let table = client
            .query(&sql)
            .await
            .expect("chrome main-thread hotspots query must succeed on page_loads.pftrace");

        // Structure check only when rows are present — row count is not asserted.
        for i in 0..table.len() {
            assert!(table.cell(i, "id").is_some(), "row {i} missing id");
            assert!(table.cell(i, "ts").is_some(), "row {i} missing ts");
            assert!(table.cell(i, "upid").is_some(), "row {i} missing upid");
            assert!(table.cell(i, "pid").is_some(), "row {i} missing pid");
            assert!(table.cell(i, "name").is_some(), "row {i} missing name");
            assert!(
                table.cell(i, "thread_name").is_some(),
                "row {i} missing thread_name",
            );
            assert!(table.cell(i, "dur_ms").is_some(), "row {i} missing dur_ms",);
        }

        let windowed_sql = chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
            page_load_id: Some(1),
            phase: Some(perfetto_mcp_rs::params::ChromeMainThreadHotspotsPhase::NavigationToFcp),
            min_dur_ms: Some(0.0),
            ..Default::default()
        })
        .expect("windowed hotspots SQL builder must succeed");
        client
            .query(&windowed_sql)
            .await
            .expect("windowed chrome main-thread hotspots query must succeed");

        let navigation_windowed_sql =
            chrome_main_thread_hotspots_sql(ChromeMainThreadHotspotsFilters {
                navigation_id: Some(7),
                phase: Some(
                    perfetto_mcp_rs::params::ChromeMainThreadHotspotsPhase::NavigationToFcp,
                ),
                min_dur_ms: Some(0.0),
                ..Default::default()
            })
            .expect("navigation-windowed hotspots SQL builder must succeed");
        client
            .query(&navigation_windowed_sql)
            .await
            .expect("navigation-windowed chrome main-thread hotspots query must succeed");
    });
}

#[test]
fn e2e_chrome_startup_summary_sql_runs_cleanly() {
    // Neither fixture has chrome_startups data. Weak assertion: SQL executes
    // without MissingTable / MissingModule / schema error. Upgrade to strong
    // assertion when a startup-specific fixture is added.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_401);
        let trace = Path::new("tests/fixtures/scroll_jank.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let table = client
            .query(CHROME_STARTUP_SUMMARY_SQL)
            .await
            .expect("chrome startup SQL must resolve against the chrome.startups module");

        // Row count not asserted — fixture has no startup data. Field shape
        // verified only when rows exist.
        for i in 0..table.len() {
            assert!(table.cell(i, "name").is_some(), "row {i} missing name");
            assert!(
                table.cell(i, "startup_duration_ms").is_some(),
                "row {i} missing startup_duration_ms",
            );
        }
    });
}

#[test]
fn e2e_chrome_web_content_interactions_sql_runs_cleanly() {
    // Neither fixture has web content interaction data captured. Weak
    // assertion: SQL executes cleanly. Upgrade when an interaction-specific
    // fixture is added.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_701);
        let trace = Path::new("tests/fixtures/scroll_jank.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let table = client
            .query(CHROME_WEB_CONTENT_INTERACTIONS_SQL)
            .await
            .expect("chrome.web_content_interactions module must resolve");

        for i in 0..table.len() {
            assert!(
                table.cell(i, "interaction_type").is_some(),
                "row {i} missing interaction_type",
            );
            assert!(table.cell(i, "dur_ms").is_some(), "row {i} missing dur_ms",);
        }
    });
}

#[test]
fn e2e_chrome_preflight_distinguishes_chrome_vs_non_chrome() {
    // The preflight SQL is the gate the ensure_chrome_trace helper runs
    // before any chrome_* tool touches the stdlib. If it returns 0 on a
    // non-Chrome trace (basic.perfetto-trace) but > 0 on Chrome fixtures,
    // the wrong-trace detection works. Without it, chrome.* stdlib views
    // on non-Chrome traces silently return empty rows and tools report
    // a successful "no data" outcome.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_801);
        let non_chrome = Path::new("tests/fixtures/basic.perfetto-trace");

        let client = manager
            .get_client(non_chrome)
            .await
            .expect("spawn tp_shell");
        let table = client
            .query(CHROME_TRACE_PREFLIGHT_SQL)
            .await
            .expect("preflight SQL must run cleanly");

        let count = table
            .cell(0, "n")
            .and_then(|v| v.as_i64())
            .expect("preflight must return one integer row");
        assert_eq!(
            count, 0,
            "basic.perfetto-trace is non-Chrome; preflight must return 0",
        );
    });

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_901);
        let chrome_fixture = Path::new("tests/fixtures/scroll_jank.pftrace");

        let client = manager
            .get_client(chrome_fixture)
            .await
            .expect("spawn tp_shell");
        let table = client
            .query(CHROME_TRACE_PREFLIGHT_SQL)
            .await
            .expect("preflight SQL must run cleanly on a Chrome trace");

        let count = table
            .cell(0, "n")
            .and_then(|v| v.as_i64())
            .expect("preflight must return one integer row");
        assert!(
            count > 0,
            "scroll_jank.pftrace is a Chrome trace; preflight must return > 0, got {count}",
        );
    });
}
