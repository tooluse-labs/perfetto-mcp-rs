// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

//! e2e coverage for the Chrome domain tools. Each test drives the
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

use perfetto_mcp_rs::params::{
    ChromeMainThreadHotspotsFilters, ChromePageLoadResourceHotspotsFilters,
    ChromePageLoadResourcePipelineFilters, ChromePageLoadResourceSummaryFilters,
    ChromePageLoadResourceUrlGrouping, ChromePageLoadScriptHotspotsFilters,
    ChromePageLoadWindowFilters,
};
use perfetto_mcp_rs::sql_templates::{
    chrome_main_thread_hotspots_sql, chrome_page_load_resource_hotspots_sql,
    chrome_page_load_resource_pipeline_sql, chrome_page_load_resource_summary_sql,
    chrome_page_load_resource_timing_evidence_sql, chrome_page_load_script_hotspots_sql,
    chrome_page_load_summary_sql, chrome_scroll_jank_summary_sql, chrome_startup_summary_sql,
    chrome_web_content_interactions_sql, CHROME_PAGE_LOAD_SUMMARY_COUNT_SQL,
    CHROME_PAGE_LOAD_SUMMARY_SQL, CHROME_SCROLL_JANK_SUMMARY_COUNT_SQL,
    CHROME_SCROLL_JANK_SUMMARY_SQL, CHROME_STARTUP_SUMMARY_COUNT_SQL, CHROME_STARTUP_SUMMARY_SQL,
    CHROME_TRACE_PREFLIGHT_SQL, CHROME_WEB_CONTENT_INTERACTIONS_COUNT_SQL,
    CHROME_WEB_CONTENT_INTERACTIONS_SQL,
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
        let limited = client
            .query(&chrome_scroll_jank_summary_sql(5))
            .await
            .expect("dynamic chrome scroll jank query must succeed");
        let count = client
            .query(CHROME_SCROLL_JANK_SUMMARY_COUNT_SQL)
            .await
            .expect("chrome scroll jank count query must succeed");

        assert!(
            !table.is_empty(),
            "scroll_jank.pftrace must yield at least one chrome_janky_frames row",
        );
        assert_eq!(
            limited.len(),
            5,
            "dynamic scroll jank LIMIT 5 must cap the six-row fixture",
        );
        assert_eq!(
            count.cell(0, "row_count").and_then(|v| v.as_i64()),
            Some(6),
            "scroll_jank.pftrace fixture row_count must stay pinned",
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
        let limited = client
            .query(&chrome_page_load_summary_sql(5))
            .await
            .expect("dynamic chrome page load summary query must succeed");
        let count = client
            .query(CHROME_PAGE_LOAD_SUMMARY_COUNT_SQL)
            .await
            .expect("chrome page load summary count query must succeed");

        assert!(
            !table.is_empty(),
            "page_loads.pftrace must yield at least one chrome_page_loads row",
        );
        assert_eq!(
            limited.len(),
            5,
            "dynamic page-load LIMIT 5 must cap the eight-row fixture",
        );
        assert_eq!(
            count.cell(0, "row_count").and_then(|v| v.as_i64()),
            Some(8),
            "page_loads.pftrace fixture row_count must stay pinned",
        );
        for i in 0..table.len() {
            assert!(table.cell(i, "id").is_some(), "row {i} missing id column",);
            assert!(
                table.cell(i, "navigation_id").is_some(),
                "row {i} missing navigation_id column",
            );
            assert!(table.cell(i, "url").is_some(), "row {i} missing url column",);
            assert!(
                table.cell(i, "navigation_start_ts").is_some(),
                "row {i} missing navigation_start_ts column",
            );
            assert!(
                table.cell(i, "fcp_ts").is_some(),
                "row {i} missing fcp_ts column",
            );
            assert!(
                table.cell(i, "dom_content_loaded_event_ts").is_some(),
                "row {i} missing dom_content_loaded_event_ts column",
            );
            assert!(
                table.cell(i, "load_event_ts").is_some(),
                "row {i} missing load_event_ts column",
            );
        }
    });
}

#[test]
fn e2e_chrome_page_load_resource_hotspots_sql_runs_cleanly() {
    // Weak assertion: SQL executes and preserves the advertised shape when
    // resource-like URL-bearing slices exist. The bundled fixture is primarily
    // a page-load boundary fixture, so row count is not asserted.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_251);
        let trace = Path::new("tests/fixtures/page_loads.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let sql = chrome_page_load_resource_hotspots_sql(ChromePageLoadResourceHotspotsFilters {
            window: ChromePageLoadWindowFilters {
                page_load_id: Some(1),
                phase: Some(perfetto_mcp_rs::params::ChromePageLoadPhase::NavigationToFcp),
                ..Default::default()
            },
            min_dur_ms: Some(0.0),
            ..Default::default()
        })
        .expect("resource hotspots SQL builder must succeed");
        let table = client
            .query(&sql)
            .await
            .expect("chrome page-load resource hotspots query must succeed");

        for i in 0..table.len() {
            assert!(table.cell(i, "id").is_some(), "row {i} missing id");
            assert!(table.cell(i, "ts").is_some(), "row {i} missing ts");
            assert!(
                table.cell(i, "overlap_ms").is_some(),
                "row {i} missing overlap_ms",
            );
            assert!(table.cell(i, "url").is_some(), "row {i} missing url");
        }
    });
}

#[test]
fn e2e_chrome_page_load_resource_summary_sql_runs_cleanly() {
    // Weak assertion: this pins SQL compatibility and the URL-summary response
    // shape. The fixture is not a slow-resource capture, so row count is not
    // asserted.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_256);
        let trace = Path::new("tests/fixtures/page_loads.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let sql = chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters {
            window: ChromePageLoadWindowFilters {
                page_load_id: Some(1),
                phase: Some(perfetto_mcp_rs::params::ChromePageLoadPhase::NavigationToFcp),
                ..Default::default()
            },
            min_overlap_ms: Some(0.0),
            url_grouping: Some(ChromePageLoadResourceUrlGrouping::WithoutQuery),
            ..Default::default()
        })
        .expect("resource summary SQL builder must succeed");
        let table = client
            .query(&sql)
            .await
            .expect("chrome page-load resource summary query must succeed");
        let raw_window_sql =
            chrome_page_load_resource_summary_sql(ChromePageLoadResourceSummaryFilters {
                window: ChromePageLoadWindowFilters {
                    start_ts_ns: Some(0),
                    end_ts_ns: Some(10_000_000_000),
                    ..Default::default()
                },
                min_overlap_ms: Some(0.0),
                url_grouping: Some(ChromePageLoadResourceUrlGrouping::WithoutQuery),
                limit: Some(5),
            })
            .expect("raw-window resource summary SQL builder must succeed");
        client
            .query(&raw_window_sql)
            .await
            .expect("raw-window resource summary query must succeed");
        let evidence_sql =
            chrome_page_load_resource_timing_evidence_sql(ChromePageLoadWindowFilters {
                page_load_id: Some(1),
                phase: Some(perfetto_mcp_rs::params::ChromePageLoadPhase::NavigationToFcp),
                ..Default::default()
            })
            .expect("resource evidence SQL builder must succeed");
        let evidence_table = client
            .query(&evidence_sql)
            .await
            .expect("chrome page-load resource evidence query must succeed");

        assert_eq!(
            evidence_table.len(),
            1,
            "resource evidence probe should return exactly one metadata row"
        );
        assert!(
            evidence_table
                .cell(0, "phase_breakdown_available")
                .is_some(),
            "evidence row missing phase_breakdown_available"
        );

        for i in 0..table.len() {
            assert!(
                table.cell(i, "url_key").is_some(),
                "row {i} missing url_key"
            );
            assert!(
                table.cell(i, "max_overlap_ms").is_some(),
                "row {i} missing max_overlap_ms",
            );
            assert!(
                table.cell(i, "summed_overlap_ms").is_some(),
                "row {i} missing summed_overlap_ms",
            );
            assert!(
                table.cell(i, "relation_to_navigation").is_some(),
                "row {i} missing relation_to_navigation",
            );
            assert!(
                table.cell(i, "url_origin").is_some(),
                "row {i} missing url_origin",
            );
            assert!(
                table.cell(i, "renderer_relation").is_some(),
                "row {i} missing renderer_relation",
            );
            assert!(
                table.cell(i, "example_slice_id").is_some(),
                "row {i} missing example_slice_id",
            );
        }
    });
}

#[test]
fn e2e_chrome_page_load_resource_pipeline_sql_runs_cleanly() {
    // Weak assertion: protects SQL compatibility and advertised columns. The
    // fixture may not contain a matching "main" resource, so row count is not
    // asserted.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_258);
        let trace = Path::new("tests/fixtures/page_loads.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let sql = chrome_page_load_resource_pipeline_sql(ChromePageLoadResourcePipelineFilters {
            window: ChromePageLoadWindowFilters {
                page_load_id: Some(1),
                phase: Some(perfetto_mcp_rs::params::ChromePageLoadPhase::NavigationToFcp),
                ..Default::default()
            },
            url_substring: Some("main"),
            ..Default::default()
        })
        .expect("resource pipeline SQL builder must succeed");
        let table = client
            .query(&sql)
            .await
            .expect("chrome page-load resource pipeline query must succeed");

        for i in 0..table.len() {
            assert!(
                table.cell(i, "url_key").is_some(),
                "row {i} missing url_key"
            );
            assert!(
                table.cell(i, "matched_by").is_some(),
                "row {i} missing matched_by"
            );
            assert!(
                table.cell(i, "matched_url_seed").is_some(),
                "row {i} missing matched_url_seed"
            );
            assert!(
                table.cell(i, "max_request_overlap_ms").is_some(),
                "row {i} missing max_request_overlap_ms",
            );
            assert!(
                table.cell(i, "evidence_boundary").is_some(),
                "row {i} missing evidence_boundary",
            );
        }
    });
}

#[test]
fn e2e_chrome_page_load_resource_pipeline_substring_stays_on_matched_url() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_268);
        let trace = Path::new("tests/fixtures/page_loads.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let sql = chrome_page_load_resource_pipeline_sql(ChromePageLoadResourcePipelineFilters {
            window: ChromePageLoadWindowFilters {
                page_load_id: Some(7),
                phase: Some(perfetto_mcp_rs::params::ChromePageLoadPhase::NavigationToFcp),
                ..Default::default()
            },
            url_substring: Some("Astronomy"),
            ..Default::default()
        })
        .expect("resource pipeline SQL builder must succeed");
        let table = client
            .query(&sql)
            .await
            .expect("chrome page-load resource pipeline query must succeed");

        assert!(
            !table.is_empty(),
            "fixture should contain at least one Astronomy resource row"
        );
        for i in 0..table.len() {
            let url_key = table
                .cell(i, "url_key")
                .and_then(|v| v.as_str())
                .expect("row should carry url_key");
            let example_url = table
                .cell(i, "example_url")
                .and_then(|v| v.as_str())
                .expect("row should carry example_url");
            assert!(
                url_key.contains("Astronomy") || example_url.contains("Astronomy"),
                "row {i} escaped requested URL substring: url_key={url_key:?}, example_url={example_url:?}",
            );
            assert_eq!(
                table.cell(i, "matched_by").and_then(|v| v.as_str()),
                Some("url_substring"),
                "row {i} should report substring matching as the seed"
            );
            assert_eq!(
                table.cell(i, "matched_url_seed").and_then(|v| v.as_str()),
                Some("Astronomy"),
                "row {i} should echo the matched substring"
            );
        }
    });
}

#[test]
fn e2e_chrome_page_load_script_hotspots_sql_runs_cleanly() {
    // Weak assertion: the fixture primarily protects SQL compatibility and
    // response shape. It may not contain large page-load script hotspots.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_261);
        let trace = Path::new("tests/fixtures/page_loads.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let sql = chrome_page_load_script_hotspots_sql(ChromePageLoadScriptHotspotsFilters {
            window: ChromePageLoadWindowFilters {
                page_load_id: Some(1),
                phase: Some(perfetto_mcp_rs::params::ChromePageLoadPhase::NavigationToFcp),
                ..Default::default()
            },
            min_total_ms: Some(0.0),
            ..Default::default()
        })
        .expect("script hotspots SQL builder must succeed");
        let table = client
            .query(&sql)
            .await
            .expect("chrome page-load script hotspots query must succeed");

        for i in 0..table.len() {
            assert!(table.cell(i, "url").is_some(), "row {i} missing url");
            assert!(table.cell(i, "name").is_some(), "row {i} missing name");
            assert!(
                table.cell(i, "total_wall_ms").is_some(),
                "row {i} missing total_wall_ms",
            );
            assert!(
                table.cell(i, "example_slice_id").is_some(),
                "row {i} missing example_slice_id",
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
        let dynamic = client
            .query(&chrome_startup_summary_sql(5))
            .await
            .expect("dynamic chrome startup SQL must resolve");
        let count = client
            .query(CHROME_STARTUP_SUMMARY_COUNT_SQL)
            .await
            .expect("chrome startup count SQL must resolve");

        // Row count not asserted — fixture has no startup data. Field shape
        // verified only when rows exist.
        assert_eq!(
            count.cell(0, "row_count").and_then(|v| v.as_i64()),
            Some(table.len() as i64),
            "startup count SQL must agree with fixture rows",
        );
        assert!(
            dynamic.len() <= 5,
            "dynamic startup LIMIT 5 must cap returned rows"
        );
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
        let dynamic = client
            .query(&chrome_web_content_interactions_sql(5))
            .await
            .expect("dynamic chrome.web_content_interactions SQL must resolve");
        let count = client
            .query(CHROME_WEB_CONTENT_INTERACTIONS_COUNT_SQL)
            .await
            .expect("chrome.web_content_interactions count SQL must resolve");

        assert_eq!(
            count.cell(0, "row_count").and_then(|v| v.as_i64()),
            Some(table.len() as i64),
            "web interaction count SQL must agree with fixture rows",
        );
        assert!(
            dynamic.len() <= 5,
            "dynamic interaction LIMIT 5 must cap returned rows"
        );
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
