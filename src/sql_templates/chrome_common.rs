use crate::error::{PerfettoError, MAX_ROWS};
use crate::params::{ChromePageLoadPhase, ChromePageLoadWindowFilters};
#[derive(Debug, Clone, Copy)]
pub(super) struct ChromePageLoadWindowSql {
    pub(super) phase: Option<ChromePageLoadPhase>,
    pub(super) start_ts_ns: Option<i64>,
    pub(super) end_ts_ns: Option<i64>,
}

pub(super) fn validate_chrome_page_load_window(
    filters: ChromePageLoadWindowFilters,
) -> Result<ChromePageLoadWindowSql, PerfettoError> {
    let ChromePageLoadWindowFilters {
        page_load_id,
        navigation_id,
        phase,
        start_ts_ns,
        end_ts_ns,
    } = filters;

    if let Some(id) = page_load_id {
        if id < 0 {
            return Err(PerfettoError::InvalidParam(format!(
                "page_load_id must be non-negative, got {id}"
            )));
        }
    }
    if let Some(id) = navigation_id {
        if id < 0 {
            return Err(PerfettoError::InvalidParam(format!(
                "navigation_id must be non-negative, got {id}"
            )));
        }
    }
    if page_load_id.is_some() && navigation_id.is_some() {
        return Err(PerfettoError::InvalidParam(
            "page_load_id and navigation_id are mutually exclusive".to_owned(),
        ));
    }
    if let Some(ts) = start_ts_ns {
        if ts < 0 {
            return Err(PerfettoError::InvalidParam(format!(
                "start_ts_ns must be non-negative, got {ts}"
            )));
        }
    }
    if let Some(ts) = end_ts_ns {
        if ts < 0 {
            return Err(PerfettoError::InvalidParam(format!(
                "end_ts_ns must be non-negative, got {ts}"
            )));
        }
    }
    if let (Some(start), Some(end)) = (start_ts_ns, end_ts_ns) {
        if end <= start {
            return Err(PerfettoError::InvalidParam(format!(
                "end_ts_ns must be greater than start_ts_ns, got start={start}, end={end}"
            )));
        }
    }

    let phase = phase.or_else(|| {
        (page_load_id.is_some() || navigation_id.is_some())
            .then_some(ChromePageLoadPhase::NavigationToFcp)
    });

    Ok(ChromePageLoadWindowSql {
        phase,
        start_ts_ns,
        end_ts_ns,
    })
}

pub(super) fn chrome_page_load_phase_columns(
    phase: ChromePageLoadPhase,
) -> (&'static str, &'static str) {
    match phase {
        ChromePageLoadPhase::NavigationToFcp => ("navigation_start_ts", "fcp_ts"),
        ChromePageLoadPhase::NavigationToLoad => ("navigation_start_ts", "load_event_ts"),
        ChromePageLoadPhase::DclToFcp => ("dom_content_loaded_event_ts", "fcp_ts"),
        ChromePageLoadPhase::FcpToLoad => ("fcp_ts", "load_event_ts"),
    }
}

pub(super) fn append_chrome_page_load_window_cte(
    sql: &mut String,
    cte_name: &str,
    filters: ChromePageLoadWindowFilters,
    window: ChromePageLoadWindowSql,
) {
    if let Some(phase) = window.phase {
        sql.push_str("INCLUDE PERFETTO MODULE chrome.page_loads; ");
        sql.push_str("WITH ");
        append_chrome_page_load_window_cte_body(sql, cte_name, filters, phase);
    }
}

pub(super) fn append_chrome_page_load_window_cte_body(
    sql: &mut String,
    cte_name: &str,
    filters: ChromePageLoadWindowFilters,
    phase: ChromePageLoadPhase,
) {
    let (start_expr, end_expr) = chrome_page_load_phase_columns(phase);
    sql.push_str(&format!(
        "{cte_name} AS ( \
         SELECT \
           navigation_start_ts AS navigation_start_ts, \
           url AS url, \
           {start_expr} AS start_ts, \
           {end_expr} AS end_ts, \
           ({end_expr} - {start_expr}) AS phase_dur_ns \
         FROM chrome_page_loads "
    ));
    if let Some(id) = filters.page_load_id {
        sql.push_str(&format!("WHERE id = {id} "));
    }
    if let Some(id) = filters.navigation_id {
        sql.push_str(&format!("WHERE navigation_id = {id} "));
    }
    sql.push_str("ORDER BY navigation_start_ts DESC LIMIT 1) ");
}

pub(super) fn duration_ms_to_ns(
    field_name: &str,
    value_ms: Option<f64>,
    default_ns: i64,
) -> Result<i64, PerfettoError> {
    match value_ms {
        None => Ok(default_ns),
        Some(ms) => {
            let ns = ms * 1_000_000.0;
            if !(ns.is_finite() && ns >= 0.0 && ns <= i64::MAX as f64) {
                return Err(PerfettoError::InvalidParam(format!(
                    "{field_name} must be finite, non-negative, and ≤ ~9.2e12 ms, got {ms}"
                )));
            }
            Ok(ns as i64)
        }
    }
}

pub(super) fn chrome_tool_row_limit(limit: Option<u32>) -> Result<u32, PerfettoError> {
    chrome_tool_row_limit_with_default(limit, 100)
}

pub(super) fn chrome_tool_row_limit_with_default(
    limit: Option<u32>,
    default_limit: u32,
) -> Result<u32, PerfettoError> {
    match limit {
        None => Ok(default_limit),
        Some(0) => Err(PerfettoError::InvalidParam("limit must be > 0".to_owned())),
        Some(n) if (n as usize) > MAX_ROWS => Ok(MAX_ROWS as u32),
        Some(n) => Ok(n),
    }
}
