// What the page says about the data as a whole, from the results of this viewing: when it was
// queried and which stretch of time it covers. Both are read from the results as they are, never
// computed from the spec.
import type { DashboardData } from "../../dashboard/data.ts";
import { mergeTimeRanges, resultTimeRange, type TimeRange } from "../../dashboard/range.ts";

/** The latest `queried_at` of the results shown, or `null` while none is in. */
export function latestQueriedAt(data: DashboardData): string | null {
  let latest: { at: string; ms: number } | null = null;
  for (const state of Object.values(data.results)) {
    if (state.status !== "ready") continue;
    const at = state.result.meta.queried_at;
    const ms = Date.parse(at);
    if (Number.isNaN(ms)) continue;
    if (latest === null || ms > latest.ms) latest = { at, ms };
  }
  return latest?.at ?? null;
}

/**
 * The first and the last time in the results' time columns, as the wall-clock times the queries
 * wrote (frontend/src/dashboard/format.ts), or `null` when no result has a time. `dateOnly` when
 * every time is a date without a time of day. One dataset's own range is `resultTimeRange`.
 */
export function dataTimeRange(data: DashboardData): TimeRange | null {
  return mergeTimeRanges(
    Object.values(data.results).map((state) => (state.status === "ready" ? resultTimeRange(state.result) : null)),
  );
}
