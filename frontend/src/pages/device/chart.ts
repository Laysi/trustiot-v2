// What the history chart plots and says, without React: which value each bucket gives the line,
// where the readings have gaps, and the figures above the chart. historyOption.ts hands the
// result to the dashboard's chart; HistoryCard.tsx shows the figures.
import type { Reading, SeriesPoint } from "../../api/devices.ts";
import { type Gap, gapLimit, gapsBetween } from "../../dashboard/chart/gaps.ts";

export type { Gap };

/** One bucket as the chart plots it. */
export interface Datum {
  /** The bucket's start, as an instant. */
  time: number;
  /** The line's value: the bucket's mean, or its last reading when it has no mean. */
  value: number;
  min: number | null;
  max: number | null;
  count: number;
}

/** A reading as a number: a bool is 0 or 1; text, and a float that was NaN or infinite, has none. */
export function readingNumber(reading: Reading): number | null {
  switch (reading.type) {
    case "float":
    case "int":
      return reading.value === null || !Number.isFinite(reading.value) ? null : reading.value;
    case "bool":
      return reading.value ? 1 : 0;
    case "string":
      return null;
  }
}

function finite(value: number | null | undefined): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/** The buckets the chart can plot, in time order; text-only buckets are left out. */
export function chartData(points: readonly SeriesPoint[]): Datum[] {
  const data: Datum[] = [];
  for (const point of points) {
    const value = finite(point.avg) ?? readingNumber(point.last);
    const time = Date.parse(point.time);
    if (value === null || !Number.isFinite(time)) continue;
    data.push({ time, value, min: finite(point.min), max: finite(point.max), count: point.count });
  }
  return data.sort((a, b) => a.time - b.time);
}

/**
 * The spans of `[from, to)` in which no bucket has readings and that are longer than three of
 * the series' usual spacing: three buckets, or three of the median spacing when the device
 * reports less often than the interval. Shorter holes are noise in the line, not a gap worth
 * naming. The answer omits buckets without readings, so this is where the chart breaks its line
 * and draws the hatched span; one at the start or the end (a device that stopped reporting)
 * counts too. The rule is the dashboard charts' too (`dashboard/chart/gaps.ts`).
 */
export function gapsOf(data: readonly Datum[], intervalMs: number | undefined, from: number, to: number): Gap[] {
  return gapsBetween(
    data.map((datum) => datum.time),
    intervalMs ?? 0,
    from,
    to,
  );
}

/** The total length of the parts of `gaps` that lie inside `[from, to)`. */
export function gapTotal(gaps: readonly Gap[], from: number, to: number): number {
  let total = 0;
  for (const gap of gaps) total += Math.max(0, Math.min(gap.to, to) - Math.max(gap.from, from));
  return total;
}

/** The figures over a range of buckets. */
export interface Summary {
  /** The mean weighted by each bucket's readings. */
  mean: number;
  /** The smallest reading, and the start of the bucket it fell in. */
  min: { value: number; time: number };
  max: { value: number; time: number };
  count: number;
}

export function summary(data: readonly Datum[]): Summary | null {
  const first = data[0];
  if (first === undefined) return null;
  let min = { value: first.min ?? first.value, time: first.time };
  let max = { value: first.max ?? first.value, time: first.time };
  let weighted = 0;
  let weight = 0;
  let count = 0;
  for (const datum of data) {
    const low = datum.min ?? datum.value;
    const high = datum.max ?? datum.value;
    if (low < min.value) min = { value: low, time: datum.time };
    if (high > max.value) max = { value: high, time: datum.time };
    const share = Math.max(1, datum.count);
    weighted += datum.value * share;
    weight += share;
    count += datum.count;
  }
  return { mean: weighted / weight, min, max, count };
}

/** The buckets whose start is in `[from, to)`. */
export function within(data: readonly Datum[], from: number, to: number): Datum[] {
  return data.filter((datum) => datum.time >= from && datum.time < to);
}

/** What a bool point's range says: where it is now, how long it was on, how often it came on. */
export interface BoolSummary {
  /** The last reading's state, or null without a bool reading. */
  current: boolean | null;
  /** The start of the bucket in which the state last changed; null when it holds for the whole range. */
  since: number | null;
  /** The share of readings that were on (0 to 1), or null without readings. */
  share: number | null;
  /** How many times the last reading of a bucket was on after being off in the bucket before. */
  turnedOn: number;
  readings: number;
}

export function boolSummary(points: readonly SeriesPoint[]): BoolSummary {
  const buckets = points
    .flatMap((point) => {
      const time = Date.parse(point.time);
      return point.last.type === "bool" && Number.isFinite(time)
        ? [{ time, on: point.last.value, avg: finite(point.avg), count: point.count }]
        : [];
    })
    .sort((a, b) => a.time - b.time);
  const lastBucket = buckets[buckets.length - 1];
  let weighted = 0;
  let weight = 0;
  let readings = 0;
  let turnedOn = 0;
  buckets.forEach((bucket, index) => {
    readings += bucket.count;
    if (bucket.avg !== null) {
      const share = Math.max(1, bucket.count);
      weighted += bucket.avg * share;
      weight += share;
    }
    if (index > 0 && bucket.on && !(buckets[index - 1] as { on: boolean }).on) turnedOn += 1;
  });
  let since: number | null = null;
  if (lastBucket !== undefined) {
    let index = buckets.length - 1;
    while (index > 0 && (buckets[index - 1] as { on: boolean }).on === lastBucket.on) index -= 1;
    since = index === 0 ? null : (buckets[index] as { time: number }).time;
  }
  return {
    current: lastBucket?.on ?? null,
    since,
    share: weight > 0 ? weighted / weight : null,
    turnedOn,
    readings,
  };
}

/** A stretch of the same text, as the table of a text point lists it. */
export interface Period {
  value: string;
  /** The first bucket's start, and the end of the last (an instant). */
  from: number;
  to: number;
  /** How many buckets the stretch spans. */
  buckets: number;
}

/**
 * The text point's buckets as periods: consecutive buckets whose last reading is the same text
 * are one row (a day of one-minute buckets is a few rows, not 1440), newest first. A hole between
 * two buckets longer than a gap (`gapsOf`) ends the period: the same text on both sides of a
 * stretch without readings is not one uninterrupted period. A reading of another type (a number
 * stored under the same key before the model changed) is its own text.
 */
export function periods(points: readonly SeriesPoint[], intervalMs: number | undefined): Period[] {
  const length = intervalMs ?? 0;
  const rows: Period[] = [];
  const sorted = points
    .map((point) => ({ point, time: Date.parse(point.time) }))
    .filter(({ time }) => Number.isFinite(time))
    .sort((a, b) => a.time - b.time);
  const limit = gapLimit(
    length,
    sorted.map(({ time }) => time),
  );
  for (const { point, time } of sorted) {
    const value = String(point.last.value);
    const current = rows[rows.length - 1];
    if (current !== undefined && current.value === value && time - current.to <= limit) {
      current.to = time + length;
      current.buckets += 1;
    } else {
      rows.push({ value, from: time, to: time + length, buckets: 1 });
    }
  }
  return rows.reverse();
}
