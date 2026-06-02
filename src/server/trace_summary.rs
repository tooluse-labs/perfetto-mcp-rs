use std::path::Path;

use serde::Serialize;

use crate::query::DecodedTable;
use crate::sql_templates::{LOAD_TRACE_METADATA_SQL, LOAD_TRACE_OVERVIEW_SQL};
use crate::tp_manager::{loaded_name_matches, strip_size_suffix};

use super::response::{current_redaction_policy, RedactionPolicy};
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(super) struct LoadTraceSummary {
    pub(super) available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) trace_type: Option<String>,
    pub(super) trace_profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) android_build_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) android_sdk_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) chrome_product_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) start_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) end_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) file_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) process_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) thread_count: Option<i64>,
    pub(super) capabilities: Vec<String>,
    pub(super) recommended_next_tools: Vec<String>,
    pub(super) redaction_policy: RedactionPolicy,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) warnings: Vec<String>,
}

#[tracing::instrument(
    level = "debug",
    name = "load_trace.summary",
    skip_all,
    fields(trace_path_len = trace_path.len(), available = tracing::field::Empty)
)]
pub(super) async fn collect_load_trace_summary(
    client: &crate::tp_client::TraceProcessorClient,
    trace_path: &str,
) -> Result<LoadTraceSummary, String> {
    let metadata = client
        .query(LOAD_TRACE_METADATA_SQL)
        .await
        .map_err(|e| format!("metadata query failed: {e}"))?;
    let overview = client
        .query(LOAD_TRACE_OVERVIEW_SQL)
        .await
        .map_err(|e| format!("overview query failed: {e}"))?;
    let file_size_bytes = std::fs::metadata(trace_path).map(|m| m.len()).ok();

    let summary = build_load_trace_summary(&metadata, &overview, file_size_bytes);
    tracing::Span::current().record("available", summary.available);
    Ok(summary)
}

pub(super) fn build_load_trace_summary(
    metadata: &DecodedTable,
    overview: &DecodedTable,
    file_size_bytes: Option<u64>,
) -> LoadTraceSummary {
    let trace_type = metadata_string(metadata, "trace_type");
    let android_build_fingerprint = metadata_string(metadata, "android_build_fingerprint");
    let android_sdk_version = metadata_i64(metadata, "android_sdk_version");
    let chrome_product_version = metadata_string(metadata, "cr-product-version")
        .or_else(|| metadata_string(metadata, "cr-2-product-version"));

    let system_name = metadata_string(metadata, "system_name");
    let system_machine = metadata_string(metadata, "system_machine");
    let chrome_os_name = metadata_string(metadata, "cr-os-name")
        .or_else(|| metadata_string(metadata, "cr-2-os-name"));
    let platform = infer_platform(
        android_build_fingerprint.as_deref(),
        android_sdk_version,
        chrome_os_name.as_deref(),
        system_name.as_deref(),
        system_machine.as_deref(),
    );

    let start_ts = overview_i64(overview, "start_ts");
    let end_ts = overview_i64(overview, "end_ts");
    let duration_ns = overview_i64(overview, "duration_ns")
        .filter(|duration_ns| *duration_ns >= 0)
        .or_else(|| match (start_ts, end_ts) {
            (Some(start), Some(end)) if end >= start => Some(end - start),
            _ => None,
        });

    let has_chrome = overview_bool(overview, "has_chrome");
    let is_android = android_build_fingerprint.is_some()
        || android_sdk_version.is_some()
        || chrome_os_name.as_deref() == Some("Android");
    let trace_profile = if has_chrome {
        "chrome"
    } else if is_android {
        "android"
    } else if trace_type.is_some() || start_ts.is_some() || end_ts.is_some() {
        "generic"
    } else {
        "unknown"
    }
    .to_owned();

    let mut capabilities = Vec::new();
    push_capability(&mut capabilities, has_chrome, "chrome");
    push_capability(&mut capabilities, is_android, "android");
    push_capability(
        &mut capabilities,
        overview_bool(overview, "has_sched"),
        "sched",
    );
    push_capability(
        &mut capabilities,
        overview_bool(overview, "has_ftrace"),
        "ftrace",
    );
    push_capability(
        &mut capabilities,
        overview_bool(overview, "has_slices"),
        "slices",
    );
    push_capability(
        &mut capabilities,
        overview_bool(overview, "has_counters"),
        "counters",
    );

    let recommended_next_tools = recommended_tools(&trace_profile);
    let mut warnings = Vec::new();
    if duration_ns.is_none() {
        warnings.push("trace duration unavailable from trace_dur()".to_owned());
    }
    if overview_i64(overview, "process_count").is_none() {
        warnings.push("process count unavailable".to_owned());
    }
    if overview_i64(overview, "thread_count").is_none() {
        warnings.push("thread count unavailable".to_owned());
    }

    LoadTraceSummary {
        available: true,
        trace_type,
        trace_profile,
        platform,
        android_build_fingerprint,
        android_sdk_version,
        chrome_product_version,
        start_ts,
        end_ts,
        duration_ms: duration_ns.map(ns_to_ms),
        file_size_bytes,
        process_count: overview_i64(overview, "process_count"),
        thread_count: overview_i64(overview, "thread_count"),
        capabilities,
        recommended_next_tools,
        redaction_policy: current_redaction_policy(),
        warnings,
    }
}

pub(super) fn format_load_trace_response(
    display: &str,
    summary: Result<LoadTraceSummary, String>,
) -> Result<String, String> {
    let display_len = display.len();
    let span = tracing::debug_span!(
        "mcp.response_shape",
        response = "load_trace",
        display_len,
        summary_available = tracing::field::Empty,
    );
    let _entered = span.enter();
    let summary_json = match summary {
        Ok(summary) => {
            tracing::Span::current().record("summary_available", true);
            serde_json::to_string(&summary)
                .map_err(|e| format!("Failed to serialize load summary: {e}"))?
        }
        Err(error) => {
            tracing::Span::current().record("summary_available", false);
            serde_json::to_string(&serde_json::json!({
                "available": false,
                "error": error,
                "redaction_policy": current_redaction_policy(),
            }))
            .map_err(|e| format!("Failed to serialize load summary: {e}"))?
        }
    };

    Ok(format!(
        "Trace loaded successfully: {display}\n\
         Trace summary: {summary_json}\n\
         Routing hint: use `recommended_next_tools` from the summary first; \
         use `list_tables` / `list_table_structure` for schema discovery and \
         `execute_sql` for custom PerfettoSQL."
    ))
}

pub(super) fn metadata_string(table: &DecodedTable, key: &str) -> Option<String> {
    for row_idx in 0..table.len() {
        if table.cell(row_idx, "name").and_then(|v| v.as_str()) == Some(key) {
            return table
                .cell(row_idx, "str_value")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned);
        }
    }
    None
}

pub(super) fn metadata_i64(table: &DecodedTable, key: &str) -> Option<i64> {
    for row_idx in 0..table.len() {
        if table.cell(row_idx, "name").and_then(|v| v.as_str()) == Some(key) {
            return table.cell(row_idx, "int_value").and_then(|v| v.as_i64());
        }
    }
    None
}

pub(super) fn overview_i64(table: &DecodedTable, column: &str) -> Option<i64> {
    table.cell(0, column).and_then(|v| v.as_i64())
}

pub(super) fn overview_bool(table: &DecodedTable, column: &str) -> bool {
    overview_i64(table, column).unwrap_or(0) != 0
}

pub(super) fn infer_platform(
    android_build_fingerprint: Option<&str>,
    android_sdk_version: Option<i64>,
    chrome_os_name: Option<&str>,
    system_name: Option<&str>,
    system_machine: Option<&str>,
) -> Option<String> {
    if android_build_fingerprint.is_some()
        || android_sdk_version.is_some()
        || chrome_os_name == Some("Android")
    {
        return Some("Android".to_owned());
    }

    let system_name = system_name.filter(|s| !s.is_empty())?;
    match system_machine.filter(|s| !s.is_empty()) {
        Some(machine) => Some(format!("{system_name} ({machine})")),
        None => Some(system_name.to_owned()),
    }
}

pub(super) fn push_capability(capabilities: &mut Vec<String>, enabled: bool, name: &str) {
    if enabled {
        capabilities.push(name.to_owned());
    }
}

pub(super) fn recommended_tools(trace_profile: &str) -> Vec<String> {
    let tools = match trace_profile {
        "chrome" => [
            "chrome_page_load_summary",
            "chrome_page_load_resource_summary",
            "chrome_page_load_resource_pipeline",
            "chrome_page_load_resource_hotspots",
            "chrome_page_load_script_hotspots",
            "chrome_scroll_jank_summary",
            "chrome_main_thread_hotspots",
            "chrome_web_content_interactions",
            "list_processes",
            "execute_sql",
        ]
        .as_slice(),
        "android" => [
            "list_stdlib_modules",
            "list_processes",
            "list_threads_in_process",
            "execute_sql",
        ]
        .as_slice(),
        _ => [
            "list_tables",
            "list_table_structure",
            "list_processes",
            "execute_sql",
        ]
        .as_slice(),
    };

    tools.iter().map(|tool| (*tool).to_owned()).collect()
}

pub(super) fn ns_to_ms(ns: i64) -> f64 {
    ((ns as f64 / 1_000_000.0) * 1000.0).round() / 1000.0
}

/// Render the load confirmation. If `trace_processor_shell`'s `/status` reports
/// a name that differs from the filesystem path we loaded — typically because
/// the trace's recording embedded a different name — surface both so users do
/// not mistake it for the wrong file loading.
pub(super) fn format_loaded_trace_display(
    trace_path: &str,
    loaded_trace_name: Option<&[u8]>,
) -> String {
    let Some(loaded) = loaded_trace_name else {
        return trace_path.to_string();
    };
    if loaded_name_matches(loaded, Path::new(trace_path)) {
        trace_path.to_string()
    } else {
        let loaded_lossy = String::from_utf8_lossy(loaded);
        format!(
            "{trace_path} (recorded as '{}')",
            strip_size_suffix(&loaded_lossy)
        )
    }
}
