use std::path::Path;

use perfetto_mcp_rs::params::{HummerT2DetailFilters, HummerT2ResultFilters};
use perfetto_mcp_rs::sql_templates::{hummer_t2_detail_sql, hummer_t2_result_sql};
use perfetto_mcp_rs::tp_manager::TraceProcessorManager;

#[test]
fn e2e_hummer_t2_queries_return_empty_on_non_t2_fixture() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let manager = TraceProcessorManager::new_with_starting_port(1, 19_801);
        let trace = Path::new("tests/fixtures/basic.perfetto-trace");
        let client = manager.get_client(trace).await.expect("spawn tp_shell");

        let result_sql = hummer_t2_result_sql(HummerT2ResultFilters {
            process_name: "com.example.app",
        })
        .expect("result SQL must build");
        let result = client
            .query(&result_sql)
            .await
            .expect("result SQL must run on a non-T2 trace");
        assert!(
            result.is_empty(),
            "basic fixture should not produce a Hummer T2 result"
        );

        let detail_sql = hummer_t2_detail_sql(HummerT2DetailFilters {
            process_name: "com.example.app",
            limit: Some(5),
            tail_window_ms: None,
            thread_state_available: false,
            sched_available: false,
        })
        .expect("detail SQL must build");
        let detail = client
            .query(&detail_sql)
            .await
            .expect("detail SQL must run on a non-T2 trace");
        assert!(
            detail.is_empty(),
            "basic fixture should not produce Hummer T2 detail rows"
        );
    });
}
