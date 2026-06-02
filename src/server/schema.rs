use crate::params::ColumnInfo;
use crate::query::DecodedTable;
/// Project one row of a `PRAGMA table_info('foo')` result into a typed
/// `ColumnInfo`. Surfaces missing `name` / `type` columns as errors —
/// SQLite's PRAGMA contract guarantees them, so absence indicates upstream
/// decoder or trace_processor drift worth surfacing rather than silently
/// rendering a placeholder. `notnull` defaults to 0 (= `nullable: true`)
/// because exotic vtables can legitimately produce NULL there, and
/// "nullable until proven otherwise" is the conservative read.
pub(super) fn pragma_row_to_column_info(
    table: &DecodedTable,
    i: usize,
) -> Result<ColumnInfo, String> {
    let name = table
        .cell(i, "name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!("PRAGMA table_info row {i} missing `name` column — SQLite contract violation")
        })?
        .to_owned();
    let data_type = table
        .cell(i, "type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!("PRAGMA table_info row {i} missing `type` column — SQLite contract violation")
        })?
        .to_owned();
    let nullable = table
        .cell(i, "notnull")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        == 0;
    Ok(ColumnInfo {
        name,
        data_type,
        nullable,
    })
}
