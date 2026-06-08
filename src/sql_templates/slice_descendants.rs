use crate::error::{PerfettoError, MAX_ROWS};
use crate::params::SliceDescendantsBreakdownFilters;
pub const DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS: f64 = 1.0;
pub const DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH: u32 = 8;
pub const DEFAULT_SLICE_DESCENDANTS_LIMIT: u32 = 100;
pub const MAX_SLICE_DESCENDANTS_ROOTS: usize = 100;

/// SQL builder for `slice_descendants_breakdown`.
///
/// The query deliberately qualifies `d.depth` / `s.*` everywhere because the
/// `slice` table itself has a `depth` column; unqualified recursive CTEs are
/// a common source of `ambiguous column name: depth` errors in agent-written
/// follow-up analysis.
///
/// `example_slice_id` picks the longest-duration descendant in each
/// (root_id, depth, name) group (ties broken by smallest `slice.id`) so that
/// `include_args=true` surfaces args from the most diagnostically interesting
/// sample, not just the lowest-id one. `first_ts_ns` keeps the name in
/// nanoseconds because `slice.ts` is a wall-clock-ish nanosecond stamp; the
/// other duration columns are in ms, so the unit suffix makes the difference
/// visible to callers.
///
/// `inclusive_total_ms` sums Perfetto's inclusive slice durations and must not
/// be summed across depths. `self_ms` subtracts direct child inclusive durations
/// per descendant slice before grouping, clamped at zero, so it is the safer
/// first column for "where is the work" attribution.
pub fn slice_descendants_breakdown_sql(
    filters: SliceDescendantsBreakdownFilters<'_>,
) -> Result<String, PerfettoError> {
    let SliceDescendantsBreakdownFilters {
        slice_ids,
        min_dur_ms,
        max_depth,
        include_args,
        row_limit,
    } = filters;

    if slice_ids.is_empty() {
        return Err(PerfettoError::InvalidParam(
            "slice_ids must contain at least one root slice id".to_owned(),
        ));
    }
    // Value-shape checks come before size checks so callers get the actionable
    // error (negative id) rather than a misleading "too many roots" when they
    // pass a long list with a single bad value.
    if let Some(id) = slice_ids.iter().find(|id| **id < 0) {
        return Err(PerfettoError::InvalidParam(format!(
            "slice ids must be non-negative, got {id}"
        )));
    }
    let deduped_slice_ids = dedupe_preserving_order(slice_ids);
    if deduped_slice_ids.len() > MAX_SLICE_DESCENDANTS_ROOTS {
        return Err(PerfettoError::InvalidParam(format!(
            "slice_ids accepts at most {MAX_SLICE_DESCENDANTS_ROOTS} roots, got {}",
            deduped_slice_ids.len()
        )));
    }
    if row_limit == 0 {
        return Err(PerfettoError::InvalidParam(
            "row_limit must be > 0; resolve via slice_descendants_effective_limit".to_owned(),
        ));
    }

    let min_dur_ms = min_dur_ms.unwrap_or(DEFAULT_SLICE_DESCENDANTS_MIN_DUR_MS);
    let min_dur_ns = {
        let ns = min_dur_ms * 1_000_000.0;
        if !(ns.is_finite() && ns >= 0.0 && ns <= i64::MAX as f64) {
            return Err(PerfettoError::InvalidParam(format!(
                "min_dur_ms must be finite, non-negative, and ≤ ~9.2e12 ms, got {min_dur_ms}"
            )));
        }
        ns as i64
    };

    let max_depth = match max_depth {
        None => DEFAULT_SLICE_DESCENDANTS_MAX_DEPTH,
        Some(0) => {
            return Err(PerfettoError::InvalidParam(
                "max_depth must be > 0 when set".to_owned(),
            ));
        }
        Some(n) if n > 64 => {
            return Err(PerfettoError::InvalidParam(format!(
                "max_depth must be <= 64 to bound recursive expansion, got {n}"
            )));
        }
        Some(n) => n,
    };
    let roots = deduped_slice_ids
        .iter()
        .map(|id| format!("({id})"))
        .collect::<Vec<_>>()
        .join(", ");
    let args_column = if include_args {
        ", \
         (SELECT group_concat( \
             a.flat_key || '=' || COALESCE( \
               a.display_value, \
               CAST(a.int_value AS TEXT), \
               CAST(a.real_value AS TEXT), \
               a.string_value, \
               '' \
             ), \
             '; ' \
           ) \
          FROM slice ex \
          JOIN args a ON a.arg_set_id = ex.arg_set_id \
          WHERE ex.id = grouped.example_slice_id) AS example_args"
    } else {
        ""
    };

    // ROW_NUMBER() in `ranked` picks the longest-duration descendant per
    // (root_id, depth, name) group with `rn = 1`; ties break on smallest
    // slice.id so the choice is deterministic across re-runs. The aggregate
    // step in `grouped` then uses MAX(CASE WHEN rn=1 THEN id) to surface
    // that representative without colliding with the SUM/MAX(dur) aggregates.
    Ok(format!(
        "WITH RECURSIVE \
           roots(root_id) AS (VALUES {roots}), \
           descendants(root_id, slice_id, depth) AS ( \
             SELECT r.root_id, s.id AS slice_id, 0 AS depth \
             FROM roots r \
             JOIN slice s ON s.id = r.root_id \
             UNION ALL \
             SELECT d.root_id, child.id AS slice_id, d.depth + 1 AS depth \
             FROM descendants d \
             JOIN slice child ON child.parent_id = d.slice_id \
             WHERE d.depth < {max_depth} \
           ), \
           ranked AS ( \
             SELECT \
               d.root_id AS root_id, \
               d.depth AS depth, \
               s.name AS name, \
               s.id AS slice_id, \
               s.dur AS dur, \
               MAX( \
                 s.dur - COALESCE(( \
                   SELECT SUM(child.dur) \
                   FROM slice child \
                   WHERE child.parent_id = s.id \
                     AND child.dur > 0 \
                 ), 0), \
                 0 \
               ) AS self_dur, \
               s.ts AS ts, \
               ROW_NUMBER() OVER ( \
                 PARTITION BY d.root_id, d.depth, s.name \
                 ORDER BY s.dur DESC, s.id ASC \
               ) AS rn \
             FROM descendants d \
             JOIN slice s ON s.id = d.slice_id \
             WHERE d.depth > 0 \
               AND s.dur >= {min_dur_ns} \
           ), \
           grouped AS ( \
             SELECT \
               root_id, \
               depth, \
               name, \
               COUNT(*) AS slice_count, \
               SUM(dur) / 1e6 AS inclusive_total_ms, \
               SUM(self_dur) / 1e6 AS self_ms, \
               MAX(dur) / 1e6 AS max_ms, \
               MIN(ts) AS first_ts_ns, \
               MAX(CASE WHEN rn = 1 THEN slice_id END) AS example_slice_id \
             FROM ranked \
             GROUP BY root_id, depth, name \
           ) \
         SELECT \
           grouped.root_id, \
           grouped.depth, \
           grouped.name, \
           grouped.slice_count, \
           ROUND(grouped.inclusive_total_ms, 3) AS inclusive_total_ms, \
           ROUND(grouped.self_ms, 3) AS self_ms, \
           ROUND(grouped.max_ms, 3) AS max_ms, \
           grouped.first_ts_ns, \
           grouped.example_slice_id{args_column} \
         FROM grouped \
         ORDER BY grouped.inclusive_total_ms DESC, grouped.max_ms DESC, \
                  grouped.slice_count DESC, grouped.root_id, grouped.depth, grouped.name \
         LIMIT {row_limit}"
    ))
}

/// Stable dedupe for slice id lists. Shared between the SQL builder (so the
/// recursive CTE seeds each root exactly once) and the handler (so the
/// pre-query that detects missing roots issues exactly one lookup per id).
pub fn dedupe_preserving_order(ids: &[i64]) -> Vec<i64> {
    let mut seen = std::collections::HashSet::with_capacity(ids.len());
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

pub fn slice_descendants_effective_limit(limit: Option<u32>) -> Result<u32, PerfettoError> {
    match limit {
        None => Ok(DEFAULT_SLICE_DESCENDANTS_LIMIT),
        Some(0) => Err(PerfettoError::InvalidParam(
            "limit must be > 0 when set".to_owned(),
        )),
        Some(n) if (n as usize) > MAX_ROWS => Ok(MAX_ROWS as u32),
        Some(n) => Ok(n),
    }
}
