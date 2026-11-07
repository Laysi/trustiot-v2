import { describe, expect, test } from "bun:test";

import type { Point } from "../model.ts";
import { gapLimit, gapsBetween, lineLayout } from "./gaps.ts";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

/** Points at `times`, each valued by its position. */
function points(times: readonly number[]): Point[] {
  return times.map((x, row) => ({ x, y: row, row }));
}

/** Hourly times from `from` to `to` (hours, inclusive). */
function hourly(from: number, to: number): number[] {
  return Array.from({ length: to - from + 1 }, (_, index) => (from + index) * HOUR);
}

describe("gapLimit", () => {
  test("is three of the median spacing, or of the length when that is longer, and at least three seconds", () => {
    expect(gapLimit(0, hourly(0, 5))).toBe(3 * HOUR);
    // One far outlier does not move the median.
    expect(gapLimit(0, [...hourly(0, 5), 100 * HOUR])).toBe(3 * HOUR);
    expect(gapLimit(HOUR, [0, MINUTE, 2 * MINUTE])).toBe(3 * HOUR);
    expect(gapLimit(0, [])).toBe(3000);
    expect(gapLimit(0, [0, 10])).toBe(3000);
  });
});

describe("gapsBetween", () => {
  test("a hole longer than three times the spacing is a gap; three times is not", () => {
    const edge = hourly(0, 4);
    expect(gapsBetween([...edge, 7 * HOUR], 0, 0, 7 * HOUR)).toEqual([]);
    expect(gapsBetween([...edge, 7 * HOUR + 1], 0, 0, 7 * HOUR + 1)).toEqual([{ from: 4 * HOUR, to: 7 * HOUR + 1 }]);
  });

  test("a hole at either end of the range counts, with the range given", () => {
    expect(gapsBetween(hourly(10, 12), 0, 0, 12 * HOUR)).toEqual([{ from: 0, to: 10 * HOUR }]);
    expect(gapsBetween(hourly(0, 2), 0, 0, 12 * HOUR)).toEqual([{ from: 2 * HOUR, to: 12 * HOUR }]);
  });

  test("no times is one gap over the whole range, and an empty range has none", () => {
    expect(gapsBetween([], 0, 5, 50)).toEqual([{ from: 5, to: 50 }]);
    expect(gapsBetween([], 0, 5, 5)).toEqual([]);
  });
});

describe("lineLayout", () => {
  test("puts a break in the middle of each hole longer than three times the median spacing", () => {
    // Hourly, then nothing for ten hours (a gateway switched off), then hourly again.
    const times = [...hourly(0, 5), ...hourly(15, 17)];
    const layout = lineLayout({ points: points(times) });
    expect(layout.gaps).toEqual([{ from: 5 * HOUR, to: 15 * HOUR }]);
    expect(layout.entries.map((entry) => entry.y)).toEqual([0, 1, 2, 3, 4, 5, null, 6, 7, 8]);
    expect(layout.entries[6]).toEqual({ x: 10 * HOUR, y: null, index: -1 });
    // Each point knows its place among the entries, a break has no point.
    expect(layout.entryOfPoint).toEqual([0, 1, 2, 3, 4, 5, 7, 8, 9]);
    expect(layout.entries.filter((entry) => entry.index >= 0).map((entry) => entry.x)).toEqual(times);
  });

  test("a series breaks at its own gaps, by its own spacing", () => {
    // Every 15 minutes: a one hour hole is a gap here, and not for a series that reports hourly.
    const quick = [0, 15, 30, 45, 120, 135].map((minute) => minute * MINUTE);
    const slow = [0, 60, 120, 180, 240, 300].map((minute) => minute * MINUTE);
    expect(lineLayout({ points: points(quick) }).gaps).toEqual([{ from: 45 * MINUTE, to: 120 * MINUTE }]);
    expect(lineLayout({ points: points(slow) }).gaps).toEqual([]);
  });

  test("keeps every hole inside the spacing as one line", () => {
    const times = [0, 1, 2, 3, 4, 7, 8].map((hour) => hour * HOUR);
    const layout = lineLayout({ points: points(times) });
    expect(layout.gaps).toEqual([]);
    expect(layout.entries.map((entry) => entry.index)).toEqual([0, 1, 2, 3, 4, 5, 6]);
    expect(layout.entryOfPoint).toEqual([0, 1, 2, 3, 4, 5, 6]);
  });

  test("shows a series of one or two points as it is", () => {
    for (const times of [[], [HOUR], [0, 100 * HOUR]]) {
      const layout = lineLayout({ points: points(times) });
      expect(layout.gaps).toEqual([]);
      expect(layout.entries.map((entry) => entry.x)).toEqual(times);
    }
  });

  test("invents no gap before the first point or after the last", () => {
    const layout = lineLayout({ points: points(hourly(100, 105)) });
    expect(layout.gaps).toEqual([]);
    expect(layout.entries.length).toBe(6);
  });

  test("a series that breaks its own line is left as it is", () => {
    const own = [
      { x: 0, y: 1, row: 0 },
      { x: HOUR, y: 1, row: 1 },
      { x: 2 * HOUR, y: 1, row: 2 },
      { x: 10 * HOUR, y: null, row: -1 },
      { x: 50 * HOUR, y: 1, row: 3 },
    ];
    const layout = lineLayout({ points: own, ownBreaks: true });
    expect(layout.gaps).toEqual([]);
    expect(layout.entries.map((entry) => entry.index)).toEqual([0, 1, 2, 3, 4]);
  });
});
