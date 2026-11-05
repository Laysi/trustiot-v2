// Whether a device's data is coming in, from what the existing APIs hold: the newest stored
// reading of each point and the device's polling interval (no health API: Laysi/trustiot-v2#454).
import type { Latest } from "../../api/devices.ts";

const MINUTE = 60_000;

/**
 * How old the newest reading may be before the data counts as stalled (decision of 2026-11-01):
 * three polling intervals, but at least two minutes, so a device polled every few seconds does
 * not flicker on one slow cycle.
 */
export function staleAfterMs(pollIntervalMs: number): number {
  return Math.max(3 * pollIntervalMs, 2 * MINUTE);
}

export type DataHealth =
  /** The latest values are not known yet (loading, or the request failed). */
  | { kind: "unknown" }
  /** Nothing was stored in the raw period (`Latest.earliest` on). */
  | { kind: "none" }
  | { kind: "ok"; newest: number }
  | { kind: "stale"; newest: number };

/** The newest reading's time over all points, or null without readings. */
export function newestReading(latest: Latest): number | null {
  let newest: number | null = null;
  for (const reading of latest.points) {
    const time = Date.parse(reading.time);
    if (Number.isFinite(time) && (newest === null || time > newest)) newest = time;
  }
  return newest;
}

export function dataHealth(latest: Latest | undefined, pollIntervalMs: number, now: number): DataHealth {
  if (latest === undefined) return { kind: "unknown" };
  const newest = newestReading(latest);
  if (newest === null) return { kind: "none" };
  return now - newest > staleAfterMs(pollIntervalMs) ? { kind: "stale", newest } : { kind: "ok", newest };
}
