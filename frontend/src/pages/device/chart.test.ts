import { describe, expect, test } from "bun:test";

import type { SeriesPoint } from "../../api/devices.ts";
import {
  bandPath,
  chartData,
  type Datum,
  linePath,
  nearestIndex,
  niceTicks,
  readingNumber,
  scales,
  segments,
  summary,
  timeTicks,
  valueAxis,
} from "./chart.ts";
import { wallClock } from "./time.ts";

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

describe("axes", () => {
  test("value ticks are round numbers that cover the data", () => {
    expect(niceTicks(0.3, 9.6, 5)).toEqual({ ticks: [0, 2, 4, 6, 8, 10], lo: 0, hi: 10 });
    expect(niceTicks(229.4, 231.2, 5)).toEqual({ ticks: [229, 229.5, 230, 230.5, 231, 231.5], lo: 229, hi: 231.5 });
    // A flat line gets a range around it instead of dividing by zero.
    const flat = niceTicks(5, 5, 5);
    expect(flat.lo).toBeLessThan(5);
    expect(flat.hi).toBeGreaterThan(5);
    // Steps of 0.1 do not drift into 0.30000000000000004.
    expect(niceTicks(0, 0.5, 5).ticks).toEqual([0, 0.1, 0.2, 0.3, 0.4, 0.5]);
  });

  test("the value axis covers each bucket's minimum and maximum; a bool axis is 0 to 1", () => {
    const data = [datum(0, 10, { min: 4, max: 12 }), datum(1, 11, { min: 9, max: 19 })];
    const axis = valueAxis(data, "number");
    expect(axis.lo).toBeLessThanOrEqual(4);
    expect(axis.hi).toBeGreaterThanOrEqual(19);
    expect(valueAxis(data, "bool")).toEqual({ ticks: [0, 0.5, 1], lo: 0, hi: 1 });
  });

  test("time ticks fall on whole local hours in the user's zone", () => {
    const from = at("2026-10-30T00:13:00Z");
    const to = from + 24 * HOUR;
    for (const zone of ["Asia/Taipei", "Asia/Kolkata", "UTC"]) {
      const { ticks, step } = timeTicks(from, to, 8, zone);
      expect(step).toBe(3 * HOUR);
      expect(ticks.length).toBeGreaterThanOrEqual(7);
      for (const tick of ticks) {
        expect(tick).toBeGreaterThanOrEqual(from);
        expect(tick).toBeLessThan(to);
        const wall = wallClock(tick, zone);
        expect(wall.minute).toBe(0);
        expect(wall.hour % 3).toBe(0);
      }
    }
  });

  test("day ticks fall on local midnights", () => {
    const { ticks, step } = timeTicks(at("2026-10-01T05:00:00Z"), at("2026-10-31T05:00:00Z"), 10, "Asia/Taipei");
    expect(step).toBeGreaterThanOrEqual(24 * HOUR);
    for (const tick of ticks) {
      expect(wallClock(tick, "Asia/Taipei").hour).toBe(0);
    }
  });
});

describe("segments", () => {
  test("a gap longer than three buckets breaks the line", () => {
    const start = at("2026-10-30T00:00:00Z");
    const data = [0, 1, 2, 3, 10, 11].map((minute) => datum(start + minute * MINUTE, minute));
    expect(segments(data, MINUTE).map((run) => run.length)).toEqual([4, 2]);
  });

  test("a device polled less often than the interval keeps one line", () => {
    // Readings every 10 minutes in 1-minute buckets: the usual spacing is 10 minutes, not a gap.
    const start = at("2026-10-30T00:00:00Z");
    const data = [0, 10, 20, 30, 40].map((minute) => datum(start + minute * MINUTE, minute));
    expect(segments(data, MINUTE).map((run) => run.length)).toEqual([5]);
  });
});

describe("paths", () => {
  const frame = { left: 40, right: 640, top: 10, bottom: 210 };
  const scale = scales(frame, { from: 0, to: 600 }, { lo: 0, hi: 100 });

  test("one line command per bucket, from the left edge to the right", () => {
    const run = [datum(0, 0), datum(300, 50), datum(600, 100)];
    expect(linePath(run, scale)).toBe("M40,210 L340,110 L640,10");
  });

  test("the band goes along the maxima and back along the minima; none without spread", () => {
    const run = [datum(0, 50, { min: 40, max: 60 }), datum(600, 50, { min: 30, max: 70 })];
    expect(bandPath(run, scale)).toBe("M40,90 L640,70 L640,150 L40,130 Z");
    expect(bandPath([datum(0, 5), datum(600, 6)], scale)).toBeNull();
    expect(bandPath([datum(0, 5, { min: null, max: null }), datum(600, 6)], scale)).toBeNull();
  });

  test("the bucket nearest to a time", () => {
    const data = [datum(0, 1), datum(100, 2), datum(200, 3)];
    expect(nearestIndex(data, -50)).toBe(0);
    expect(nearestIndex(data, 40)).toBe(0);
    expect(nearestIndex(data, 60)).toBe(1);
    expect(nearestIndex(data, 1000)).toBe(2);
    expect(nearestIndex([], 0)).toBe(-1);
  });

  test("the summary weights each bucket's mean by its readings", () => {
    const data = [datum(0, 10, { min: 8, max: 12, count: 3 }), datum(1, 20, { min: 15, max: 25, count: 1 })];
    expect(summary(data)).toEqual({ min: 8, max: 25, mean: 12.5, count: 4 });
    expect(summary([])).toBeNull();
  });
});
