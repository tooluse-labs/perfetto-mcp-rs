// Copyright 2025 The perfetto-mcp-rs Authors
// SPDX-License-Identifier: Apache-2.0

//! E2E coverage for `slice_descendants_breakdown`'s recursive CTE against a
//! real trace. This pins the `slice.parent_id` traversal shape that agents
//! otherwise tend to hand-write, often with ambiguous `depth` columns.

use std::path::Path;

use perfetto_mcp_rs::params::SliceDescendantsBreakdownFilters;
use perfetto_mcp_rs::sql_templates::slice_descendants_breakdown_sql;
use perfetto_mcp_rs::tp_manager::TraceProcessorManager;

const SLICE_DESCENDANTS_PORT: u16 = 20_101;

#[test]
fn e2e_slice_descendants_breakdown_against_fixture() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, SLICE_DESCENDANTS_PORT);
        let trace = Path::new("tests/fixtures/page_loads.pftrace");

        let client = manager.get_client(trace).await.expect("spawn tp_shell");
        let roots = client
            .query(
                "SELECT parent_id AS root_id \
                 FROM slice \
                 WHERE parent_id IS NOT NULL \
                   AND dur >= 0 \
                 LIMIT 1",
            )
            .await
            .expect("fixture must expose slice.parent_id");
        let root_id = roots
            .cell(0, "root_id")
            .and_then(|v| v.as_i64())
            .expect("page_loads fixture must contain at least one child slice");

        let sql = slice_descendants_breakdown_sql(SliceDescendantsBreakdownFilters {
            slice_ids: &[root_id],
            min_dur_ms: Some(0.0),
            max_depth: Some(8),
            include_args: true,
            row_limit: 50,
        })
        .expect("slice descendants SQL builder must succeed");
        let table = client
            .query(&sql)
            .await
            .expect("slice descendants query must run against the fixture");

        assert!(
            !table.is_empty(),
            "fixture root {root_id} should yield at least one descendants group",
        );
        // Pin the column set once, against the schema — checking presence in
        // `columns` rather than `cell(row, col).is_some()` because the latter
        // would silently pass on a NULL cell, leaving an args-column rename
        // or accidental drop undetected.
        let expected_columns = [
            "root_id",
            "depth",
            "name",
            "slice_count",
            "inclusive_total_ms",
            "self_ms",
            "max_ms",
            "first_ts_ns",
            "example_slice_id",
            "example_args",
        ];
        for col in expected_columns {
            assert!(
                table.columns.iter().any(|c| c == col),
                "result must expose {col}; got columns={:?}",
                table.columns,
            );
        }
        for i in 0..table.len() {
            for col in [
                "root_id",
                "depth",
                "name",
                "slice_count",
                "inclusive_total_ms",
                "self_ms",
            ] {
                let cell = table
                    .cell(i, col)
                    .unwrap_or_else(|| panic!("row {i} missing {col}"));
                assert!(!cell.is_null(), "row {i} {col} must be non-null: {cell:?}");
            }
            // example_slice_id is the longest-dur descendant per group; the
            // fixture has at least one matching slice so a non-null id is
            // guaranteed when slice_count > 0.
            let example = table
                .cell(i, "example_slice_id")
                .expect("example_slice_id column present");
            assert!(
                !example.is_null(),
                "row {i} example_slice_id must be non-null when descendants exist: {example:?}",
            );
        }
    });
}
