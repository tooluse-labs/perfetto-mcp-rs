use rmcp::model::Resource;

use crate::params::ListStdlibModulesParams;
use crate::stdlib_catalog::{
    STDLIB_MODULE_LIST, STDLIB_QUICKREF, STDLIB_QUICKREF_MIME_TYPE, STDLIB_QUICKREF_URI,
};
pub(super) fn stdlib_quickref_resource() -> Resource {
    Resource::new(STDLIB_QUICKREF_URI, "stdlib-quickref")
        .with_title("PerfettoSQL stdlib quick reference")
        .with_description("Curated PerfettoSQL stdlib modules and minimal routing examples.")
        .with_mime_type(STDLIB_QUICKREF_MIME_TYPE)
        .with_size(STDLIB_QUICKREF.len() as u64)
}

pub(super) fn filtered_stdlib_modules_json(
    params: &ListStdlibModulesParams,
) -> Result<String, String> {
    if params.domain.is_none() && params.query.is_none() && params.limit.is_none() {
        return Ok(STDLIB_MODULE_LIST.to_owned());
    }

    let domain = params
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    if let Some(domain) = domain.as_deref() {
        if !matches!(domain, "chrome" | "android" | "generic") {
            return Err(format!(
                "`domain` must be one of chrome, android, generic; got {domain:?}"
            ));
        }
    }

    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());

    let limit = match params.limit {
        Some(0) => return Err("`limit` must be > 0 when set.".to_owned()),
        Some(n) => Some(n as usize),
        None => None,
    };

    let modules: Vec<serde_json::Value> = serde_json::from_str(STDLIB_MODULE_LIST)
        .map_err(|e| format!("Failed to parse stdlib module catalog: {e}"))?;
    let iter = modules.into_iter().filter(|entry| {
        let domain_matches = domain
            .as_deref()
            .is_none_or(|domain| entry.get("domain").and_then(|v| v.as_str()) == Some(domain));
        let query_matches = query
            .as_deref()
            .is_none_or(|query| stdlib_module_entry_matches(entry, query));
        domain_matches && query_matches
    });
    let filtered: Vec<_> = match limit {
        Some(limit) => iter.take(limit).collect(),
        None => iter.collect(),
    };

    serde_json::to_string(&filtered).map_err(|e| format!("Failed to serialize results: {e}"))
}

fn stdlib_module_entry_matches(entry: &serde_json::Value, query: &str) -> bool {
    for key in ["domain", "module", "description"] {
        if entry
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.to_ascii_lowercase().contains(query))
        {
            return true;
        }
    }
    entry
        .get("views")
        .and_then(|v| v.as_array())
        .is_some_and(|views| {
            views.iter().any(|view| {
                view.as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().contains(query))
            })
        })
}
