import { describe, expect, test } from "bun:test";

import type { SeriesPoint } from "../../api/devices.ts";
import {
  boolSummary,
  chartData,
  type Datum,
  gapsOf,
  gapTotal,
  periods,
  readingNumber,
  summary,
  within,
} from "./chart.ts";

const at = (iso: string) => Date.parse(iso);
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

function datum(time: number, value: number, extra: Partial<Datum> = {}): Datum {
  return { time, value, min: value, max: value, count: 1, ...extra };
}

describe("chartData", () => {
  test("plots the mean, or the last reading when there is no mean, and leaves text out", () => {
    const points: SeriesPoint[] = [
      { time: "2026-10-30T00:01:00Z", count: 2, last: { type: "float", value: 2 }, min: 1, max: 3, avg: 2 },
      { time: "2026-10-30T00:00:00Z", count: 1, last: { type: "int", value: 7 } },
      { time: "2026-10-30T00:02:00Z", count: 1, last: { type: "string", value: "fault" }, avg: null },
      { time: "2026-10-30T00:03:00Z", count: 1, last: { type: "float", value: null }, avg: null },
    ];
    expect(chartData(points)).toEqual([
      { time: at("2026-10-30T00:00:00Z"), value: 7, min: null, max: null, count: 1 },
      { time: at("2026-10-30T00:01:00Z"), value: 2, min: 1, max: 3, count: 2 },
    ]);
  });

  test("a bool reading is 0 or 1; NaN and text are not numbers", () => {
    expect(readingNumber({ type: "bool", value: true })).toBe(1);
    expect(readingNumber({ type: "bool", value: false })).toBe(0);
    expect(readingNumber({ type: "float", value: null })).toBeNull();
    expect(readingNumber({ type: "string", value: "1" })).toBeNull();
  });
});

describe("gaps", () => {
  const start = at("2026-11-01T00:00:00Z");
  const minutes = (from: number, to: number) =>
    Array.from({ length: to - from }, (_, index) => datum(start + (from + index) * MINUTE, 1));

  test("a hole longer than three buckets is a gap that runs from the end of one bucket to the start of the next", () => {
    const data = [...minutes(0, 10), ...minutes(20, 30)];
    expect(gapsOf(data, MINUTE, start, start + 30 * MINUTE)).toEqual([
      { from: start + 10 * MINUTE, to: start + 20 * MINUTE },
    ]);
  });

  test("a hole of up to three buckets is noise, not a gap", () => {
    const data = [...minutes(0, 10), ...minutes(13, 20)];
    expect(gapsOf(data, MINUTE, start, start + 20 * MINUTE)).toEqual([]);
  });

  test("a device that stopped reporting has a gap from its last bucket to the end of the range", () => {
    expect(gapsOf(minutes(0, 10), MINUTE, start, start + 3 * HOUR)).toEqual([
      { from: start + 10 * MINUTE, to: start + 3 * HOUR },
    ]);
  });

  test("a range that starts before the first reading has a gap at its start", () => {
    expect(gapsOf(minutes(60, 70), MINUTE, start, start + 70 * MINUTE)).toEqual([
      { from: start, to: start + 60 * MINUTE },
    ]);
  });

  test("a device that reports less often than the interval is judged by its own spacing", () => {
    // One reading every 10 minutes in one-minute buckets: no gap, until a reading is missed three times over.
    const sparse = [0, 10, 20, 30, 40, 100].map((minute) => datum(start + minute * MINUTE, 1));
    expect(gapsOf(sparse, MINUTE, start, start + 101 * MINUTE)).toEqual([
      { from: start + 41 * MINUTE, to: start + 100 * MINUTE },
    ]);
  });

  test("without any bucket the whole range is one gap, and an empty range has none", () => {
    expect(gapsOf([], MINUTE, start, start + HOUR)).toEqual([{ from: start, to: start + HOUR }]);
    expect(gapsOf([], MINUTE, start, start)).toEqual([]);
  });

  test("the total counts only the part inside the window", () => {
    const gaps = [{ from: start + 10 * MINUTE, to: start + 20 * MINUTE }];
    expect(gapTotal(gaps, start, start + HOUR)).toBe(10 * MINUTE);
    expect(gapTotal(gaps, start + 15 * MINUTE, start + HOUR)).toBe(5 * MINUTE);
    expect(gapTotal(gaps, start + 30 * MINUTE, start + HOUR)).toBe(0);
  });
});

describe("summary", () => {
  test("the extremes come with the bucket they fell in, and the mean is weighted by readings", () => {
    const data = [
      datum(1000, 10, { min: 8, max: 12, count: 1 }),
      datum(2000, 20, { min: 15, max: 30, count: 3 }),
      datum(3000, 14, { min: 14, max: 14, count: 1 }),
    ];
    expect(summary(data)).toEqual({
      mean: (10 * 1 + 20 * 3 + 14 * 1) / 5,
      min: { value: 8, time: 1000 },
      max: { value: 30, time: 2000 },
      count: 5,
    });
    expect(summary([])).toBeNull();
  });

  test("buckets are taken by their start inside the window", () => {
    const data = [datum(1000, 1), datum(2000, 2), datum(3000, 3)];
    expect(within(data, 2000, 3000).map((item) => item.time)).toEqual([2000]);
  });
});

describe("bool summary", () => {
  const bucket = (minute: number, on: boolean, share: number, count = 6): SeriesPoint => ({
    time: new Date(at("2026-11-01T00:00:00Z") + minute * MINUTE).toISOString(),
    count,
    last: { type: "bool", value: on },
    avg: share,
    min: share === 0 ? 0 : 0,
    max: share === 0 ? 0 : 1,
  });

  test("the state now, since when, the share on and how often it came on", () => {
    const points = [
      bucket(0, false, 0),
      bucket(1, true, 0.5),
      bucket(2, true, 1),
      bucket(3, false, 0),
      bucket(4, true, 1),
      bucket(5, true, 1),
    ];
    const figures = boolSummary(points);
    expect(figures.current).toBe(true);
    expect(figures.since).toBe(at("2026-11-01T00:04:00Z"));
    expect(figures.turnedOn).toBe(2);
    expect(figures.readings).toBe(36);
    expect(figures.share).toBeCloseTo((0 + 0.5 + 1 + 0 + 1 + 1) / 6);
  });

  test("a state that holds for the whole range has no start", () => {
    const figures = boolSummary([bucket(0, true, 1), bucket(1, true, 1)]);
    expect(figures.current).toBe(true);
    expect(figures.since).toBeNull();
    expect(figures.turnedOn).toBe(0);
  });

  test("without a bool reading there is no state", () => {
    expect(boolSummary([])).toEqual({ current: null, since: null, share: null, turnedOn: 0, readings: 0 });
  });
});

describe("periods", () => {
  const text = (minute: number, value: string): SeriesPoint => ({
    time: new Date(at("2026-11-01T00:00:00Z") + minute * MINUTE).toISOString(),
    count: 2,
    last: { type: "string", value },
  });

  test("consecutive buckets with the same text are one row, newest first", () => {
    const rows = periods(
      [text(0, "ok"), text(1, "ok"), text(2, "trip"), text(3, "ok"), text(4, "ok"), text(5, "ok")],
      MINUTE,
    );
    expect(rows.map((row) => [row.value, row.buckets])).toEqual([
      ["ok", 3],
      ["trip", 1],
      ["ok", 2],
    ]);
    const first = rows[2];
    expect(first?.from).toBe(at("2026-11-01T00:00:00Z"));
    expect(first?.to).toBe(at("2026-11-01T00:02:00Z"));
    expect(rows[0]?.to).toBe(at("2026-11-01T00:06:00Z"));
  });

  test("the same text on both sides of a gap is two periods", () => {
    const rows = periods([text(0, "ok"), text(1, "ok"), text(2, "ok"), text(60, "ok"), text(61, "ok")], MINUTE);
    expect(rows.map((row) => [row.buckets, row.from])).toEqual([
      [2, at("2026-11-01T01:00:00Z")],
      [3, at("2026-11-01T00:00:00Z")],
    ]);
  });

  test("buckets arriving out of order are sorted first", () => {
    expect(periods([text(2, "b"), text(0, "a"), text(1, "a")], MINUTE).map((row) => row.value)).toEqual(["b", "a"]);
  });
});
