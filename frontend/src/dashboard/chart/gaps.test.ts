import { describe, expect, test } from "bun:test";

import type { Point } from "../model.ts";
import type { ChartTheme } from "./engine.ts";
import { type Gap, gapLabelSeries, gapLimit, gapMarkArea, gapsBetween, lineLayout } from "./gaps.ts";

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

describe("the hatch and the names of the gaps", () => {
  const theme: ChartTheme = {
    series: [],
    text: "black",
    muted: "gray",
    grid: "lightgray",
    axis: "gray",
    surface: "white",
    font: "sans-serif",
  };
  // A range of 100 hours, so an hour is one percent of it.
  const RANGE = { from: 0, to: 100 * HOUR };
  const at = (start: number, share: number): Gap => ({ from: start * HOUR, to: (start + share) * HOUR });
  const area = (gaps: Gap[], label: (gap: Gap) => string | null = () => "no readings") => ({
    gaps,
    theme,
    ...RANGE,
    x: (value: number) => value,
    label,
    text: (text: string) => text,
  });
  type Area = {
    z?: number;
    data: Array<[{ xAxis: number; label?: { position: string; formatter(): string } }, { xAxis: number }]>;
  };
  const named = (gaps: Gap[], label?: (gap: Gap) => string | null) =>
    (gapLabelSeries(area(gaps, label), "s0")?.markArea as Area | undefined)?.data.map(([start, end]) => ({
      from: start.xAxis,
      to: end.xAxis,
      position: start.label?.position,
      text: start.label?.formatter(),
    }));

  test("the hatch covers every gap and carries no name", () => {
    const hatch = gapMarkArea(area([at(10, 1), at(30, 20)])) as { label: { show: boolean }; data: unknown[][] };
    expect(hatch.data.length).toBe(2);
    expect(hatch.label.show).toBe(false);
    expect(JSON.stringify(hatch.data)).not.toContain("label");
  });

  test("a gap of at least 2.5 % of the range is named, a narrower one is hatched only", () => {
    expect(named([at(10, 2.4)])).toBeUndefined();
    expect(named([at(10, 2.5)])?.map((item) => item.text)).toEqual(["no readings"]);
    expect(named([at(10, 2.4), at(40, 30)])?.map((item) => item.from)).toEqual([40 * HOUR]);
  });

  test("a gap the caller gives no text is not named", () => {
    const gaps = [at(10, 20), at(50, 20)];
    expect(named(gaps, (gap) => (gap === gaps[1] ? "second" : null))?.map((item) => item.text)).toEqual(["second"]);
    expect(named(gaps, () => null)).toBeUndefined();
  });

  test("a name at an end of the range is kept inside it, one in the middle is centered", () => {
    expect(named([at(0, 10)])?.[0]?.position).toBe("insideTopLeft");
    expect(named([at(90, 10)])?.[0]?.position).toBe("insideTopRight");
    expect(named([at(40, 10)])?.[0]?.position).toBe("insideTop");
  });

  test("a gap whose end does not come after its start on the axis is neither hatched nor named", () => {
    const flat = { ...area([at(10, 20)]), x: () => 5 };
    expect((gapMarkArea(flat) as { data: unknown[] }).data).toEqual([]);
    expect(gapLabelSeries(flat, "s0")).toBeNull();
  });

  test("the names are a series of their own, above the lines and the mark lines, with no data or tooltip", () => {
    const series = gapLabelSeries(area([at(10, 20)]), "s3");
    expect(series?.name).toBe("s3");
    expect(series?.data).toEqual([]);
    expect(series?.tooltip).toEqual({ show: false });
    // ECharts draws lines at z 2 and mark lines at z 5.
    expect(((series?.markArea as Area | undefined)?.z ?? 0) > 5).toBe(true);
  });
});
