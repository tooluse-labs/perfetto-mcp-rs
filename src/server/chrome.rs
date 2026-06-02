use crate::sql_templates::CHROME_TRACE_PREFLIGHT_SQL;
/// Preflight check for chrome_* tools. Without it, chrome.* stdlib views
/// on a non-Chrome trace return an empty view (not an error), and each tool
/// would report a successful "no data" outcome, making callers treat the
/// trace as a Chrome trace with no events. This check rejects upfront.
#[tracing::instrument(
    level = "debug",
    name = "chrome.preflight",
    skip(client),
    fields(tool_label = tool_label, has_chrome = tracing::field::Empty)
)]
pub(super) async fn ensure_chrome_trace(
    client: &crate::tp_client::TraceProcessorClient,
    tool_label: &str,
) -> Result<(), String> {
    let table = client
        .query(CHROME_TRACE_PREFLIGHT_SQL)
        .await
        .map_err(|e| format!("{tool_label}: preflight check failed: {e}"))?;
    let has_chrome = table.cell(0, "n").and_then(|v| v.as_i64()).unwrap_or(0);
    tracing::Span::current().record("has_chrome", has_chrome);
    if has_chrome == 0 {
        return Err(format!(
            "{tool_label} requires a Chrome-family trace, but no \
             `chrome.process_type` track-descriptor args were found in this \
             trace. Call `list_stdlib_modules` to discover modules that fit \
             this trace, then query via execute_sql."
        ));
    }
    Ok(())
}
