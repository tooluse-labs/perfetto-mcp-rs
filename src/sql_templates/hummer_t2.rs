use crate::error::{PerfettoError, MAX_ROWS};
use crate::params::{HummerT2DetailFilters, HummerT2ResultFilters};

use super::sql_string_literal;

pub const DEFAULT_HUMMER_T2_TOP_SLICE_LIMIT: u32 = 30;
pub const DEFAULT_HUMMER_T2_TAIL_WINDOW_MS: u32 = 180;

const HUMMER_T2_DETAIL_LIMITED_SECTION_COUNT: usize = 11;
const HUMMER_T2_DETAIL_FIXED_ROW_RESERVE: usize = 128;
pub const MAX_HUMMER_T2_DETAIL_SECTION_LIMIT: u32 =
    ((MAX_ROWS - HUMMER_T2_DETAIL_FIXED_ROW_RESERVE) / HUMMER_T2_DETAIL_LIMITED_SECTION_COUNT)
        as u32;

const T2_WINDOW_CTE: &str = "collector_session_window AS ( \
    SELECT \
      s.ts AS start_ts, \
      s.ts + CAST(c.value * 1000000 AS INT64) AS end_ts, \
      c.ts AS end_counter_ts, \
      c.value AS t2_ms, \
      'session' AS source \
    FROM slice s \
    JOIN counter c ON c.ts >= s.ts \
    JOIN counter_track t ON t.id = c.track_id \
    WHERE s.name = 'T2CollectorSession' \
      AND s.dur > 0 \
      AND t.name = 'T2 result milliseconds' \
      AND c.value > 0 \
      AND c.ts <= s.ts + s.dur \
    ORDER BY s.ts DESC \
    LIMIT 1 \
  ), \
  legacy_t2_window AS ( \
    SELECT \
      start_counter.ts AS start_ts, \
      start_counter.ts + CAST(end_ms.value * 1000000 AS INT64) AS end_ts, \
      end_ms.ts AS end_counter_ts, \
      end_ms.value AS t2_ms, \
      'legacy' AS source \
    FROM counter end_ms \
    JOIN counter_track end_track ON end_track.id = end_ms.track_id \
    JOIN counter start_counter ON start_counter.ts = ( \
      SELECT MAX(c.ts) \
      FROM counter c \
      JOIN counter_track t ON t.id = c.track_id \
      WHERE t.name = 'T2 start' AND c.ts <= end_ms.ts \
    ) \
    JOIN counter_track start_track ON start_track.id = start_counter.track_id \
    WHERE end_track.name = 'T2 end milliseconds' \
      AND start_track.name = 'T2 start' \
    ORDER BY end_ms.ts DESC \
    LIMIT 1 \
  ), \
  selected_t2_window AS ( \
    SELECT * FROM collector_session_window \
    UNION ALL \
    SELECT * FROM legacy_t2_window \
    WHERE NOT EXISTS (SELECT 1 FROM collector_session_window) \
  )";

fn process_overlap_cte(process_name_lit: &str) -> String {
    format!(
        "process_overlap AS ( \
           SELECT \
             SUM((MIN(s.ts + s.dur, w.end_ts) - MAX(s.ts, w.start_ts)) / 1e6) AS process_overlap_ms \
           FROM thread_slice s, selected_t2_window w \
           WHERE s.process_name = {process_name_lit} \
             AND s.dur > 0 \
             AND s.ts < w.end_ts \
             AND s.ts + s.dur > w.start_ts \
         )"
    )
}

pub fn hummer_t2_result_sql(filters: HummerT2ResultFilters<'_>) -> Result<String, PerfettoError> {
    let process_name_lit = sql_string_literal(filters.process_name)?;
    Ok(format!(
        "INCLUDE PERFETTO MODULE slices.with_context; \
         WITH {T2_WINDOW_CTE}, {process_overlap_cte} \
         SELECT \
           {process_name_lit} AS process_name, \
           w.source, \
           w.start_ts, \
           w.end_ts, \
           w.end_counter_ts, \
           w.t2_ms, \
           ROUND((w.end_ts - w.start_ts) / 1e6, 3) AS window_ms, \
           ROUND(COALESCE(process_overlap.process_overlap_ms, 0), 3) AS process_overlap_ms \
         FROM selected_t2_window w, process_overlap \
         WHERE COALESCE(process_overlap.process_overlap_ms, 0) > 0",
        process_overlap_cte = process_overlap_cte(&process_name_lit)
    ))
}

pub fn hummer_t2_detail_sql(filters: HummerT2DetailFilters<'_>) -> Result<String, PerfettoError> {
    let process_name_lit = sql_string_literal(filters.process_name)?;
    let row_limit = hummer_t2_effective_limit(filters.limit)?;
    let tail_window_ms = hummer_t2_effective_tail_window_ms(filters.tail_window_ms)?;
    let tail_window_ns = i64::from(tail_window_ms) * 1_000_000;
    let thread_state_rows_cte =
        thread_state_rows_cte(&process_name_lit, row_limit, filters.thread_state_available);
    let sched_rows_cte = sched_rows_cte(&process_name_lit, row_limit, filters.sched_available);
    let tail_nulls = "NULL AS tail_window_start_ts, NULL AS tail_window_end_ts, \
         NULL AS tail_window_ms, NULL AS rel_start_ms, NULL AS rel_end_ms, \
         NULL AS tail_overlap_ms, NULL AS pct_of_tail, NULL AS id, \
         NULL AS parent_id, NULL AS parent_name, NULL AS parent_thread_name, \
         NULL AS parent_overlap_ms";
    Ok(format!(
         "INCLUDE PERFETTO MODULE slices.with_context; \
         WITH {T2_WINDOW_CTE}, \
         {process_overlap_cte}, \
         valid_t2_window AS ( \
           SELECT 1 AS valid \
           FROM selected_t2_window w, process_overlap \
           WHERE COALESCE(process_overlap.process_overlap_ms, 0) > 0 \
         ), \
         tail_window AS ( \
           SELECT \
             MAX(w.start_ts, w.end_ts - {tail_window_ns}) AS tail_window_start_ts, \
             w.end_ts AS tail_window_end_ts, \
             ROUND((w.end_ts - MAX(w.start_ts, w.end_ts - {tail_window_ns})) / 1e6, 3) AS tail_window_ms \
           FROM selected_t2_window w \
         ), \
         image_percent AS ( \
           SELECT \
             COUNT(c.value) AS sample_count, \
             MAX(c.value) / 100.0 AS max_image_percent, \
             MIN(CASE WHEN c.value > 0 THEN (c.ts - w.start_ts) / 1e6 END) AS first_positive_rel_ms, \
             ( \
               SELECT (c2.ts - w.start_ts) / 1e6 \
               FROM counter c2 \
               JOIN counter_track t2 ON t2.id = c2.track_id \
               WHERE t2.name = 'T2 image percent' \
                 AND c2.ts BETWEEN w.start_ts AND w.end_ts \
               ORDER BY c2.value DESC, c2.ts ASC \
               LIMIT 1 \
             ) AS max_rel_ms \
           FROM selected_t2_window w \
           LEFT JOIN ( \
             SELECT c.ts, c.value \
             FROM counter c \
             JOIN counter_track t ON t.id = c.track_id \
             WHERE t.name = 'T2 image percent' \
           ) c ON c.ts BETWEEN w.start_ts AND w.end_ts \
         ), \
         image_percent_rows AS ( \
           SELECT \
             ROUND((c.ts - w.start_ts) / 1e6, 3) AS rel_ms, \
             c.ts, \
             ROUND(c.value / 100.0, 3) AS image_percent \
           FROM selected_t2_window w \
           JOIN counter c ON c.ts BETWEEN w.start_ts AND w.end_ts \
           JOIN counter_track t ON t.id = c.track_id \
           WHERE t.name = 'T2 image percent' \
             AND EXISTS (SELECT 1 FROM valid_t2_window) \
           ORDER BY c.ts \
           LIMIT {row_limit} \
         ), \
         placeholder_summary AS ( \
           SELECT COUNT(*) AS placeholder_count \
           FROM slice s, selected_t2_window w \
           WHERE s.name GLOB 'T2PlaceholderFound*' \
             AND s.ts BETWEEN w.start_ts AND w.end_ts \
         ), \
         placeholders AS ( \
           SELECT \
             s.ts, \
             ROUND((s.ts - w.start_ts) / 1e6, 3) AS rel_ms, \
             s.name \
           FROM slice s, selected_t2_window w \
           WHERE s.name GLOB 'T2PlaceholderFound*' \
             AND s.ts BETWEEN w.start_ts AND w.end_ts \
             AND EXISTS (SELECT 1 FROM valid_t2_window) \
           ORDER BY s.ts \
           LIMIT {row_limit} \
         ), \
         overlap AS ( \
           SELECT \
             CASE \
               WHEN s.name GLOB 'DecompressTexture*' \
                 OR s.name GLOB 'ImageFromCompressedData*' \
                 OR s.name GLOB 'ImageFromDecompressedData*' \
                 THEN 'image_decode' \
               WHEN s.name = 'UploadTextureToPrivate' THEN 'image_upload' \
               WHEN s.name GLOB '*ReclaimResources*' \
                 OR s.thread_name = 'IplrVkResMgr' \
                 THEN 'resource_reclaim' \
               WHEN s.thread_name = 'HeapTaskDaemon' \
                 OR s.name GLOB '*GC*' \
                 OR s.name IN ( \
                   'Scavenge', \
                   'CopyingPhase', \
                   'MarkingPhase', \
                   'ReclaimPhase', \
                   'Sweep', \
                   'SweepArray', \
                   'SweepLargeObjects', \
                   'Process mark stacks and References' \
                 ) THEN 'art_gc' \
               WHEN s.thread_name = 'DartWorker' \
                 OR s.name = 'ConcurrentMark' \
                 OR s.name GLOB 'Dart_*' \
                 THEN 'dart_runtime' \
               WHEN s.name = 'ConcurrentWorkerWake' \
                 OR s.thread_name GLOB 'io.worker*' \
                 THEN 'worker_activity' \
               WHEN s.thread_name = '1.ui' THEN 'ui_thread' \
               WHEN s.thread_name = '1.raster' THEN 'raster_thread' \
               ELSE 'other' \
             END AS category, \
             s.thread_name, \
             s.name, \
             s.id, \
             s.parent_id, \
             s.ts, \
             s.ts + s.dur AS slice_end_ts, \
             s.dur / 1e6 AS full_dur_ms, \
             (MIN(s.ts + s.dur, w.end_ts) - MAX(s.ts, w.start_ts)) / 1e6 AS overlap_ms, \
             ROUND((MAX(s.ts, w.start_ts) - w.start_ts) / 1e6, 3) AS rel_start_ms, \
             ROUND((MIN(s.ts + s.dur, w.end_ts) - w.start_ts) / 1e6, 3) AS rel_end_ms, \
             w.t2_ms \
           FROM thread_slice s, selected_t2_window w \
           WHERE s.process_name = {process_name_lit} \
             AND s.dur > 0 \
             AND s.ts < w.end_ts \
             AND s.ts + s.dur > w.start_ts \
         ), \
         category_rows AS ( \
           SELECT \
             category, \
             COUNT(*) AS slice_count, \
             ROUND(SUM(overlap_ms), 3) AS total_overlap_ms, \
             ROUND(MAX(overlap_ms), 3) AS max_overlap_ms, \
             ROUND(100.0 * SUM(overlap_ms) / MAX(t2_ms), 3) AS pct_of_t2 \
           FROM overlap \
           WHERE overlap_ms > 0 \
           GROUP BY category \
         ), \
         image_work_rows AS ( \
           SELECT \
             thread_name, \
             name, \
             COUNT(*) AS slice_count, \
             ROUND(SUM(overlap_ms), 3) AS total_overlap_ms, \
             ROUND(AVG(overlap_ms), 3) AS avg_overlap_ms, \
             ROUND(MAX(overlap_ms), 3) AS max_overlap_ms \
           FROM overlap \
           WHERE overlap_ms > 0 \
             AND category IN ('image_decode', 'image_upload') \
           GROUP BY thread_name, name \
           ORDER BY total_overlap_ms DESC \
           LIMIT {row_limit} \
         ), \
         top_slice_rows AS ( \
           SELECT \
             category, \
             thread_name, \
             name, \
             ROUND(full_dur_ms, 3) AS full_dur_ms, \
             ROUND(overlap_ms, 3) AS overlap_ms, \
             ts \
           FROM overlap \
           WHERE overlap_ms > 0 \
           ORDER BY overlap_ms DESC \
           LIMIT {row_limit} \
         ), \
         other_top_slice_rows AS ( \
           SELECT \
             category, \
             thread_name, \
             name, \
             ROUND(full_dur_ms, 3) AS full_dur_ms, \
             ROUND(overlap_ms, 3) AS overlap_ms, \
             ts \
           FROM overlap \
           WHERE overlap_ms > 0 AND category = 'other' \
           ORDER BY overlap_ms DESC \
           LIMIT 10 \
         ), \
         tail_overlap AS ( \
           SELECT \
             o.*, \
             tw.tail_window_ms, \
             (MIN(o.slice_end_ts, tw.tail_window_end_ts) - MAX(o.ts, tw.tail_window_start_ts)) / 1e6 AS tail_overlap_ms \
           FROM overlap o, tail_window tw \
           WHERE o.slice_end_ts > tw.tail_window_start_ts \
             AND o.ts < tw.tail_window_end_ts \
         ), \
         tail_category_rows AS ( \
           SELECT \
             category, \
             COUNT(*) AS slice_count, \
             ROUND(SUM(tail_overlap_ms), 3) AS total_overlap_ms, \
             ROUND(MAX(tail_overlap_ms), 3) AS max_overlap_ms, \
             ROUND(100.0 * SUM(tail_overlap_ms) / MAX(tail_window_ms), 3) AS pct_of_tail \
           FROM tail_overlap \
           WHERE tail_overlap_ms > 0 \
           GROUP BY category \
         ), \
         tail_slice_rows AS ( \
           SELECT \
             category, thread_name, name, id, parent_id, \
             rel_start_ms, rel_end_ms, \
             ROUND(full_dur_ms, 3) AS full_dur_ms, \
             ROUND(overlap_ms, 3) AS overlap_ms, \
             ROUND(tail_overlap_ms, 3) AS tail_overlap_ms, \
             ts \
           FROM tail_overlap \
           WHERE tail_overlap_ms > 0 \
           ORDER BY tail_overlap_ms DESC \
           LIMIT {row_limit} \
         ), \
         end_blocker_rows AS ( \
           SELECT \
             category, thread_name, name, id, parent_id, \
             rel_start_ms, rel_end_ms, \
             ROUND(full_dur_ms, 3) AS full_dur_ms, \
             ROUND(overlap_ms, 3) AS overlap_ms, \
             ROUND(tail_overlap_ms, 3) AS tail_overlap_ms, \
             ts \
           FROM tail_overlap \
           WHERE tail_overlap_ms > 0 \
           ORDER BY rel_end_ms DESC, tail_overlap_ms DESC \
           LIMIT {row_limit} \
         ), \
         parent_child_rows AS ( \
           SELECT \
             child.category, \
             child.thread_name, \
             child.name, \
             child.id, \
             child.parent_id, \
             parent.thread_name AS parent_thread_name, \
             parent.name AS parent_name, \
             ROUND(child.overlap_ms, 3) AS overlap_ms, \
             ROUND((MIN(parent.ts + parent.dur, w.end_ts) - MAX(parent.ts, w.start_ts)) / 1e6, 3) AS parent_overlap_ms \
           FROM tail_overlap child \
           LEFT JOIN thread_slice parent ON parent.id = child.parent_id \
           JOIN selected_t2_window w \
           WHERE child.tail_overlap_ms > 0 \
           ORDER BY child.tail_overlap_ms DESC \
           LIMIT {row_limit} \
         ), \
         contention_rows AS ( \
           SELECT \
             category, thread_name, name, id, parent_id, \
             rel_start_ms, rel_end_ms, \
             ROUND(full_dur_ms, 3) AS full_dur_ms, \
             ROUND(overlap_ms, 3) AS overlap_ms, \
             ROUND(tail_overlap_ms, 3) AS tail_overlap_ms, \
             ts \
           FROM tail_overlap \
           WHERE tail_overlap_ms > 0 \
             AND (name GLOB '*contention*' OR name GLOB '*Lock contention*') \
           ORDER BY tail_overlap_ms DESC \
           LIMIT {row_limit} \
         ), \
         thread_state_availability AS ( \
           SELECT \
             {thread_state_available} AS thread_state_available, \
             {sched_available} AS sched_available \
         ), \
         thread_counts AS ( \
           SELECT \
             COALESCE(th.name, '<unknown>') AS thread_name, \
             COUNT(s.id) AS slice_count \
           FROM thread th \
           JOIN process p ON p.upid = th.upid \
           LEFT JOIN thread_track tt ON tt.utid = th.utid \
           LEFT JOIN slice s ON s.track_id = tt.id \
           WHERE p.name = {process_name_lit} \
           GROUP BY thread_name \
         ), \
         thread_summary_rows AS ( \
           SELECT thread_name, slice_count \
           FROM thread_counts \
           WHERE EXISTS (SELECT 1 FROM valid_t2_window) \
           ORDER BY slice_count DESC \
           LIMIT {row_limit} \
         ), \
         bridge_summary AS ( \
           SELECT \
             MAX(CASE WHEN thread_name = '1.ui' AND slice_count > 0 THEN 1 ELSE 0 END) AS has_ui_thread, \
             MAX(CASE WHEN thread_name = '1.raster' AND slice_count > 0 THEN 1 ELSE 0 END) AS has_raster_thread, \
             MAX(CASE WHEN thread_name GLOB 'io.worker*' AND slice_count > 0 THEN 1 ELSE 0 END) AS has_worker_thread, \
             MAX(CASE WHEN thread_name = 'DartWorker' AND slice_count > 0 THEN 1 ELSE 0 END) AS has_dart_worker \
           FROM thread_counts \
         ), \
         {thread_state_rows_cte}, \
         {sched_rows_cte} \
         SELECT 't2_window' AS section, \
                {process_name_lit} AS process_name, \
                w.source, w.start_ts, w.end_ts, w.end_counter_ts, w.t2_ms, \
                ROUND((w.end_ts - w.start_ts) / 1e6, 3) AS window_ms, \
                ROUND(COALESCE(process_overlap.process_overlap_ms, 0), 3) AS process_overlap_ms, \
                NULL AS sample_count, NULL AS max_image_percent, NULL AS first_positive_rel_ms, NULL AS max_rel_ms, NULL AS image_percent, \
                NULL AS placeholder_count, NULL AS rel_ms, \
                NULL AS category, NULL AS slice_count, NULL AS total_overlap_ms, NULL AS avg_overlap_ms, NULL AS max_overlap_ms, NULL AS pct_of_t2, \
                NULL AS thread_name, NULL AS name, NULL AS full_dur_ms, NULL AS overlap_ms, NULL AS ts, \
                NULL AS state, NULL AS state_span_count, NULL AS state_total_ms, NULL AS state_max_ms, \
                NULL AS cpu_count, NULL AS sched_span_count, NULL AS sched_total_ms, NULL AS sched_max_ms, \
                NULL AS thread_state_available, NULL AS sched_available, \
                NULL AS has_ui_thread, NULL AS has_raster_thread, NULL AS has_worker_thread, NULL AS has_dart_worker, \
                {tail_nulls} \
         FROM selected_t2_window w, process_overlap \
         WHERE COALESCE(process_overlap.process_overlap_ms, 0) > 0 \
         UNION ALL \
         SELECT 'image_percent_summary', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                sample_count, ROUND(max_image_percent, 3), ROUND(first_positive_rel_ms, 3), ROUND(max_rel_ms, 3), NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM image_percent \
         WHERE EXISTS (SELECT 1 FROM valid_t2_window) \
         UNION ALL \
         SELECT 'image_percent', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, image_percent, \
                NULL, rel_ms, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, ts, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM image_percent_rows \
         UNION ALL \
         SELECT 'placeholder_summary', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                placeholder_count, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM placeholder_summary \
         WHERE EXISTS (SELECT 1 FROM valid_t2_window) \
         UNION ALL \
         SELECT 'placeholder', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, rel_ms, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, name, NULL, NULL, ts, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM placeholders \
         UNION ALL \
         SELECT 'category', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, category, slice_count, total_overlap_ms, NULL, max_overlap_ms, pct_of_t2, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM category_rows \
         UNION ALL \
         SELECT 'image_work', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, slice_count, total_overlap_ms, avg_overlap_ms, max_overlap_ms, NULL, \
                thread_name, name, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM image_work_rows \
         UNION ALL \
         SELECT 'top_slice', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, category, NULL, NULL, NULL, NULL, NULL, \
                thread_name, name, full_dur_ms, overlap_ms, ts, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM top_slice_rows \
         UNION ALL \
         SELECT 'other_top_slice', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, category, NULL, NULL, NULL, NULL, NULL, \
                thread_name, name, full_dur_ms, overlap_ms, ts, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM other_top_slice_rows \
         UNION ALL \
         SELECT 'tail_window', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                tail_window_start_ts, tail_window_end_ts, tail_window_ms, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL \
         FROM tail_window \
         WHERE EXISTS (SELECT 1 FROM valid_t2_window) \
         UNION ALL \
         SELECT 'tail_category', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, category, slice_count, total_overlap_ms, NULL, max_overlap_ms, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, pct_of_tail, NULL, NULL, NULL, NULL, NULL \
         FROM tail_category_rows \
         UNION ALL \
         SELECT 'tail_slice', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, category, NULL, NULL, NULL, NULL, NULL, \
                thread_name, name, full_dur_ms, overlap_ms, ts, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, rel_start_ms, rel_end_ms, tail_overlap_ms, NULL, id, parent_id, NULL, NULL, NULL \
         FROM tail_slice_rows \
         UNION ALL \
         SELECT 'end_blocker', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, category, NULL, NULL, NULL, NULL, NULL, \
                thread_name, name, full_dur_ms, overlap_ms, ts, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, rel_start_ms, rel_end_ms, tail_overlap_ms, NULL, id, parent_id, NULL, NULL, NULL \
         FROM end_blocker_rows \
         UNION ALL \
         SELECT 'parent_child', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, category, NULL, NULL, NULL, NULL, NULL, \
                thread_name, name, NULL, overlap_ms, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, id, parent_id, parent_name, parent_thread_name, parent_overlap_ms \
         FROM parent_child_rows \
         UNION ALL \
         SELECT 'contention', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, category, NULL, NULL, NULL, NULL, NULL, \
                thread_name, name, full_dur_ms, overlap_ms, ts, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, rel_start_ms, rel_end_ms, tail_overlap_ms, NULL, id, parent_id, NULL, NULL, NULL \
         FROM contention_rows \
         UNION ALL \
         SELECT 'bridge_summary', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, has_ui_thread, has_raster_thread, has_worker_thread, has_dart_worker, \
                {tail_nulls} \
         FROM bridge_summary \
         WHERE EXISTS (SELECT 1 FROM valid_t2_window) \
         UNION ALL \
         SELECT 'thread_summary', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, slice_count, NULL, NULL, NULL, NULL, \
                thread_name, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM thread_summary_rows \
         UNION ALL \
         SELECT 'thread_state_availability', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                thread_state_available, sched_available, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM thread_state_availability \
         WHERE EXISTS (SELECT 1 FROM valid_t2_window) \
         UNION ALL \
         SELECT 'thread_state', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                thread_name, NULL, NULL, NULL, NULL, \
                state, state_span_count, state_total_ms, state_max_ms, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM thread_state_rows \
         UNION ALL \
         SELECT 'sched', \
                {process_name_lit}, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
                thread_name, NULL, NULL, NULL, NULL, \
                NULL, NULL, NULL, NULL, cpu_count, sched_span_count, sched_total_ms, sched_max_ms, \
                NULL, NULL, NULL, NULL, NULL, NULL, \
                {tail_nulls} \
         FROM sched_rows",
        process_overlap_cte = process_overlap_cte(&process_name_lit),
        tail_window_ns = tail_window_ns,
        tail_nulls = tail_nulls,
        thread_state_available = if filters.thread_state_available { 1 } else { 0 },
        sched_available = if filters.sched_available { 1 } else { 0 },
        thread_state_rows_cte = thread_state_rows_cte,
        sched_rows_cte = sched_rows_cte,
    ))
}

fn thread_state_rows_cte(process_name_lit: &str, row_limit: u32, available: bool) -> String {
    if !available {
        return "thread_state_rows AS ( \
                  SELECT NULL AS thread_name, NULL AS state, NULL AS state_span_count, \
                         NULL AS state_total_ms, NULL AS state_max_ms \
                  WHERE 0 \
                )"
        .to_owned();
    }

    format!(
        "thread_state_rows AS ( \
           SELECT \
             COALESCE(th.name, '<unknown>') AS thread_name, \
             ts.state AS state, \
             COUNT(*) AS state_span_count, \
             ROUND(SUM((MIN(ts.ts + ts.dur, w.end_ts) - MAX(ts.ts, w.start_ts)) / 1e6), 3) AS state_total_ms, \
             ROUND(MAX((MIN(ts.ts + ts.dur, w.end_ts) - MAX(ts.ts, w.start_ts)) / 1e6), 3) AS state_max_ms \
           FROM thread_state ts \
           JOIN thread th ON th.utid = ts.utid \
           JOIN process p ON p.upid = th.upid \
           JOIN selected_t2_window w \
           WHERE p.name = {process_name_lit} \
             AND ts.dur > 0 \
             AND ts.ts < w.end_ts \
             AND ts.ts + ts.dur > w.start_ts \
             AND EXISTS (SELECT 1 FROM valid_t2_window) \
           GROUP BY thread_name, state \
           HAVING state_total_ms > 0 \
           ORDER BY \
             CASE \
               WHEN state = 'Running' THEN 0 \
               WHEN state GLOB 'R*' THEN 1 \
               WHEN state GLOB 'D*' THEN 2 \
               WHEN state = 'S' THEN 9 \
               ELSE 3 \
             END, \
             state_total_ms DESC \
           LIMIT {row_limit} \
         )"
    )
}

fn sched_rows_cte(process_name_lit: &str, row_limit: u32, available: bool) -> String {
    if !available {
        return "sched_rows AS ( \
                  SELECT NULL AS thread_name, NULL AS cpu_count, NULL AS sched_span_count, \
                         NULL AS sched_total_ms, NULL AS sched_max_ms \
                  WHERE 0 \
                )"
        .to_owned();
    }

    format!(
        "sched_rows AS ( \
           SELECT \
             COALESCE(th.name, '<unknown>') AS thread_name, \
             COUNT(DISTINCT sc.cpu) AS cpu_count, \
             COUNT(*) AS sched_span_count, \
             ROUND(SUM((MIN(sc.ts + sc.dur, w.end_ts) - MAX(sc.ts, w.start_ts)) / 1e6), 3) AS sched_total_ms, \
             ROUND(MAX((MIN(sc.ts + sc.dur, w.end_ts) - MAX(sc.ts, w.start_ts)) / 1e6), 3) AS sched_max_ms \
           FROM sched sc \
           JOIN thread th ON th.utid = sc.utid \
           JOIN process p ON p.upid = th.upid \
           JOIN selected_t2_window w \
           WHERE p.name = {process_name_lit} \
             AND sc.dur > 0 \
             AND sc.ts < w.end_ts \
             AND sc.ts + sc.dur > w.start_ts \
             AND EXISTS (SELECT 1 FROM valid_t2_window) \
           GROUP BY thread_name \
           HAVING sched_total_ms > 0 \
           ORDER BY sched_total_ms DESC \
           LIMIT {row_limit} \
         )"
    )
}

pub fn hummer_t2_effective_limit(limit: Option<u32>) -> Result<u32, PerfettoError> {
    match limit {
        None => Ok(DEFAULT_HUMMER_T2_TOP_SLICE_LIMIT),
        Some(0) => Err(PerfettoError::InvalidParam(
            "limit must be > 0 when set".to_owned(),
        )),
        Some(n) if n > MAX_HUMMER_T2_DETAIL_SECTION_LIMIT => Ok(MAX_HUMMER_T2_DETAIL_SECTION_LIMIT),
        Some(n) => Ok(n),
    }
}

pub fn hummer_t2_effective_tail_window_ms(
    tail_window_ms: Option<u32>,
) -> Result<u32, PerfettoError> {
    match tail_window_ms {
        None => Ok(DEFAULT_HUMMER_T2_TAIL_WINDOW_MS),
        Some(0) => Err(PerfettoError::InvalidParam(
            "tail_window_ms must be > 0 when set".to_owned(),
        )),
        Some(n) => Ok(n),
    }
}
