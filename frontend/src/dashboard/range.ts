// The stretch of time a result covers, read from its time columns as they are (never computed
// from the spec). The dashboard page's 「資料時間」 merges the datasets' ranges; the source panel
// shows one dataset's.
import type { DatasetResult } from "./data.ts";
import { isDateOnly, wallMs } from "./format.ts";

/**
 * The first and the last time of a range, as the wall-clock times the queries wrote
 * (dashboard/format.ts); `dateOnly` when every time is a date without a time of day.
 */
export interface TimeRange {
  from: number;
  to: number;
  dateOnly: boolean;
}

/** The first and the last time in the result's time columns, or `null` when it has none. */
export function resultTimeRange(result: DatasetResult): TimeRange | null {
  let from: number | null = null;
  let to: number | null = null;
  let dateOnly = true;
  for (const [index, column] of result.columns.entries()) {
    if (column.type !== "time") continue;
    for (const row of result.rows) {
      const cell = row[index] ?? null;
      const ms = wallMs(cell);
      if (ms === null) continue;
      if (!isDateOnly(cell)) dateOnly = false;
      if (from === null || ms < from) from = ms;
      if (to === null || ms > to) to = ms;
    }
  }
  return from === null || to === null ? null : { from, to, dateOnly };
}

/** The range that covers all of `ranges`; `null` entries count for nothing, and `null` when none is left. */
export function mergeTimeRanges(ranges: Iterable<TimeRange | null>): TimeRange | null {
  let merged: TimeRange | null = null;
  for (const range of ranges) {
    if (range === null) continue;
    merged =
      merged === null
        ? range
        : {
            from: Math.min(merged.from, range.from),
            to: Math.max(merged.to, range.to),
            dateOnly: merged.dateOnly && range.dateOnly,
          };
  }
  return merged;
}
