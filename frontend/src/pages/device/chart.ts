// The history chart's geometry, without React: which value each bucket plots, the axes' ticks,
// where the line breaks, and the SVG paths. TimeSeriesChart.tsx draws what this computes.
import type { Reading, SeriesPoint } from "../../api/devices.ts";
import { DURATION, fromWallClock, offsetAt } from "./time.ts";

/** One bucket as the chart plots it. */
export interface Datum {
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

/** Round numbers between `lo` and `hi`, about `target` of them, and the domain they span. */
export function niceTicks(lo: number, hi: number, target: number): { ticks: number[]; lo: number; hi: number } {
  if (!(Number.isFinite(lo) && Number.isFinite(hi))) return { ticks: [0, 1], lo: 0, hi: 1 };
  if (lo === hi) {
    // A flat line sits in the middle of a small range around it.
    const pad = lo === 0 ? 1 : Math.abs(lo) * 0.1;
    return niceTicks(lo - pad, hi + pad, target);
  }
  const raw = (hi - lo) / Math.max(1, target);
  const magnitude = 10 ** Math.floor(Math.log10(raw));
  const step = ([1, 2, 2.5, 5, 10].find((factor) => factor * magnitude >= raw) ?? 10) * magnitude;
  const start = Math.floor(lo / step) * step;
  const end = Math.ceil(hi / step) * step;
  const ticks: number[] = [];
  // Counting steps instead of adding them keeps 0.1 + 0.2 from drifting.
  for (let index = 0; start + index * step <= end + step / 2; index += 1) {
    ticks.push(Number((start + index * step).toPrecision(12)));
  }
  return { ticks, lo: start, hi: end };
}

/** The vertical domain and its ticks: 0 to 1 for a bool point (the share of readings that were on). */
export function valueAxis(data: readonly Datum[], kind: "number" | "bool", target = 5) {
  if (kind === "bool") return { ticks: [0, 0.5, 1], lo: 0, hi: 1 };
  let lo = Number.POSITIVE_INFINITY;
  let hi = Number.NEGATIVE_INFINITY;
  for (const datum of data) {
    lo = Math.min(lo, datum.min ?? datum.value, datum.value);
    hi = Math.max(hi, datum.max ?? datum.value, datum.value);
  }
  return niceTicks(lo, hi, target);
}

/**
 * Runs of buckets drawn as one line. Buckets without readings are not in the answer, so a gap
 * longer than three buckets, or than three of the series' usual spacing when the device reports
 * less often than the interval, breaks the line instead of drawing across a time with no data.
 */
export function segments(data: readonly Datum[], intervalMs: number | undefined): Datum[][] {
  if (data.length === 0) return [];
  const gaps: number[] = [];
  for (let index = 1; index < data.length; index += 1) {
    gaps.push((data[index] as Datum).time - (data[index - 1] as Datum).time);
  }
  const sorted = [...gaps].sort((a, b) => a - b);
  const median = sorted.length > 0 ? (sorted[Math.floor(sorted.length / 2)] as number) : 0;
  const limit = 3 * Math.max(intervalMs ?? 0, median);
  const runs: Datum[][] = [[data[0] as Datum]];
  for (let index = 1; index < data.length; index += 1) {
    const datum = data[index] as Datum;
    if ((gaps[index - 1] as number) > limit) {
      runs.push([datum]);
    } else {
      (runs[runs.length - 1] as Datum[]).push(datum);
    }
  }
  return runs;
}

/** Tick spacings on the time axis, finest first. */
const TIME_STEPS = [
  1,
  5,
  10,
  15,
  30,
  60,
  2 * 60,
  3 * 60,
  6 * 60,
  12 * 60,
  24 * 60,
  2 * 24 * 60,
  7 * 24 * 60,
  14 * 24 * 60,
  30 * 24 * 60,
].map((minutes) => minutes * DURATION.minute);

/**
 * Ticks on the time axis between `from` and `to`, at most about `target`, on round wall-clock
 * times in `timeZone` (whole hours, local midnights), so the labels read 09:00 and not 09:13.
 */
export function timeTicks(
  from: number,
  to: number,
  target: number,
  timeZone: string,
): { ticks: number[]; step: number } {
  const span = to - from;
  const step = TIME_STEPS.find((candidate) => span / candidate <= Math.max(1, target)) ?? span;
  const ticks: number[] = [];
  if (!(span > 0)) return { ticks, step };
  // Steps count in wall-clock time: a tick is a local time that is a whole number of steps.
  const firstLocal = Math.ceil((from + offsetAt(from, timeZone)) / step) * step;
  for (let local = firstLocal; ticks.length < 1000; local += step) {
    const instant = fromWallClock(local, timeZone);
    if (instant >= to) break;
    if (instant >= from && instant !== ticks[ticks.length - 1]) ticks.push(instant);
  }
  return { ticks, step };
}

/** Where the plot sits in the SVG. */
export interface Frame {
  left: number;
  right: number;
  top: number;
  bottom: number;
}

export interface Scales {
  x(time: number): number;
  y(value: number): number;
  /** The time at a horizontal position. */
  timeAt(x: number): number;
}

export function scales(frame: Frame, time: { from: number; to: number }, value: { lo: number; hi: number }): Scales {
  const width = frame.right - frame.left;
  const height = frame.bottom - frame.top;
  const span = Math.max(1, time.to - time.from);
  const range = value.hi - value.lo || 1;
  return {
    x: (t) => frame.left + ((t - time.from) / span) * width,
    y: (v) => frame.bottom - ((v - value.lo) / range) * height,
    timeAt: (x) => time.from + ((x - frame.left) / width) * span,
  };
}

const round = (value: number) => Math.round(value * 10) / 10;

/** The line through a run: `M` to the first bucket, then `L` to each next one. */
export function linePath(run: readonly Datum[], scale: Scales): string {
  return run
    .map((datum, index) => `${index === 0 ? "M" : "L"}${round(scale.x(datum.time))},${round(scale.y(datum.value))}`)
    .join(" ");
}

/** The band between each bucket's minimum and maximum, or null when the run has none to show. */
export function bandPath(run: readonly Datum[], scale: Scales): string | null {
  if (run.length < 2 || !run.every((datum) => datum.min !== null && datum.max !== null)) return null;
  if (run.every((datum) => datum.min === datum.max)) return null;
  const top = run.map((datum) => `${round(scale.x(datum.time))},${round(scale.y(datum.max as number))}`);
  const bottom = run
    .slice()
    .reverse()
    .map((datum) => `${round(scale.x(datum.time))},${round(scale.y(datum.min as number))}`);
  return `M${top.join(" L")} L${bottom.join(" L")} Z`;
}

/** The bucket nearest to `time` (data in time order), by binary search. */
export function nearestIndex(data: readonly Datum[], time: number): number {
  if (data.length === 0) return -1;
  let low = 0;
  let high = data.length - 1;
  while (low < high) {
    const middle = Math.floor((low + high) / 2);
    if ((data[middle] as Datum).time < time) low = middle + 1;
    else high = middle;
  }
  if (low > 0 && time - (data[low - 1] as Datum).time < (data[low] as Datum).time - time) return low - 1;
  return low;
}

/** Summary over the range: the extremes and the mean weighted by each bucket's readings. */
export function summary(data: readonly Datum[]): { min: number; max: number; mean: number; count: number } | null {
  if (data.length === 0) return null;
  let min = Number.POSITIVE_INFINITY;
  let max = Number.NEGATIVE_INFINITY;
  let weighted = 0;
  let count = 0;
  for (const datum of data) {
    min = Math.min(min, datum.min ?? datum.value);
    max = Math.max(max, datum.max ?? datum.value);
    const weight = Math.max(1, datum.count);
    weighted += datum.value * weight;
    count += weight;
  }
  return { min, max, mean: weighted / count, count: data.reduce((sum, datum) => sum + datum.count, 0) };
}
