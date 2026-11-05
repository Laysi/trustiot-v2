// The history chart's option, checked against ECharts itself where the fake engine cannot tell:
// ECharts 6 runs here with its SVG renderer in server-side mode (no canvas needed), on the same
// modules the app registers.
import { describe, expect, test } from "bun:test";
import { init, use } from "echarts/core";
import { SVGRenderer } from "echarts/renderers";

import { CHART_MODULES } from "../../dashboard/chart/echarts.ts";
import type { ChartTheme } from "../../dashboard/chart/engine.ts";
import type { ChartEnvironment } from "../../dashboard/chart/InteractiveChart.tsx";
import { withSelection } from "../../dashboard/chart/options.ts";
import { dashboardFormat } from "../../dashboard/format.ts";
import { type Datum, gapsOf } from "./chart.ts";
import { fromWall, historyModel, historyOption, toWall, wallBounds } from "./historyOption.ts";

use([...CHART_MODULES, SVGRenderer]);

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const START = Date.parse("2026-11-01T00:00:00Z");

const THEME: ChartTheme = {
  series: ["rgba(42, 120, 214, 1.000)", "rgba(212, 87, 42, 1.000)", "", "", "", ""],
  text: "rgba(23, 23, 23, 1.000)",
  muted: "rgba(98, 98, 98, 1.000)",
  grid: "rgba(229, 229, 229, 1.000)",
  axis: "rgba(163, 163, 163, 1.000)",
  surface: "rgba(255, 255, 255, 1.000)",
  font: "sans-serif",
};
const ENVIRONMENT: ChartEnvironment = { theme: THEME, animate: false, finePointer: true };

function buckets(from: number, to: number, share?: number): Datum[] {
  return Array.from({ length: to - from }, (_, index) => ({
    time: START + (from + index) * MINUTE,
    value: share ?? 230 + index * 0.1,
    min: share === undefined ? 229 + index * 0.1 : 0,
    max: share === undefined ? 231 + index * 0.1 : 1,
    count: 6,
  }));
}

function input(data: Datum[], kind: "number" | "bool", timeZone = "UTC") {
  const from = START;
  const to = START + 2 * HOUR;
  const gaps = gapsOf(data, MINUTE, from, to);
  return {
    data,
    kind,
    model: historyModel(data, gaps, timeZone),
    gaps,
    from,
    to,
    timeZone,
    gapLabel: (gap: { from: number; to: number }) => `${(gap.to - gap.from) / MINUTE} minutes without readings`,
    percent: (value: number) => `${Math.round(value * 100)}%`,
  };
}

describe("time zones", () => {
  test("an instant is the chart's x value in the user's wall clock and back again", () => {
    const instant = Date.parse("2026-11-01T05:36:00Z");
    expect(toWall(instant, "Asia/Taipei")).toBe(instant + 8 * HOUR);
    expect(fromWall(toWall(instant, "Asia/Taipei"), "Asia/Taipei")).toBe(instant);
    expect(fromWall(toWall(instant, "America/New_York"), "America/New_York")).toBe(instant);
  });
});

describe("the axis bounds", () => {
  const zone = "America/New_York";

  test("a range that crosses the daylight-saving fall-back still runs forward", () => {
    // 01:30 EDT to 01:30 EST: the wall clock shows the same time at both ends.
    const from = Date.parse("2026-11-01T05:30:00Z");
    const to = Date.parse("2026-11-01T06:30:00Z");
    expect(toWall(to, zone)).toBe(toWall(from, zone));
    const bounds = wallBounds(from, to, zone);
    expect(bounds.min).toBe(toWall(from, zone));
    expect(bounds.max - bounds.min).toBe(HOUR);
    // Ends in the repeated hour that would run backwards.
    const reversed = wallBounds(Date.parse("2026-11-01T05:50:00Z"), Date.parse("2026-11-01T06:10:00Z"), zone);
    expect(reversed.max).toBeGreaterThan(reversed.min);
  });

  test("an ordinary range keeps its wall-clock bounds", () => {
    const from = Date.parse("2026-11-03T00:00:00Z");
    const to = Date.parse("2026-11-03T06:00:00Z");
    expect(wallBounds(from, to, zone)).toEqual({ min: toWall(from, zone), max: toWall(to, zone) });
  });
});

describe("the points of the line", () => {
  test("a hole between two runs gets a null point, and the buckets keep their stops", () => {
    const data = [...buckets(0, 10), ...buckets(30, 40)];
    const gaps = gapsOf(data, MINUTE, START, START + HOUR);
    const model = historyModel(data, gaps, "UTC");
    expect(model.points.length).toBe(21);
    expect(model.points[10]).toEqual({ x: START + 20 * MINUTE, y: null, row: -1 });
    expect(model.pointOfStop.length).toBe(20);
    expect(model.pointOfStop[9]).toBe(9);
    expect(model.pointOfStop[10]).toBe(11);
    expect(model.stopOfPoint.get(11)).toBe(10);
    expect(model.stopOfPoint.has(10)).toBe(false);
  });

  test("a gap at the start or the end needs no break: nothing is drawn there", () => {
    const data = buckets(30, 40);
    const model = historyModel(data, gapsOf(data, MINUTE, START, START + 2 * HOUR), "UTC");
    expect(model.points.every((point) => point.y !== null)).toBe(true);
  });

  test("a bucket whose wall-clock time does not move forward is not drawn", () => {
    // New York's clocks go back at 06:00 UTC on 2026-11-01: 05:59 and 06:00 UTC show 01:59 and 01:00.
    const data: Datum[] = [
      { time: Date.parse("2026-11-01T05:58:00Z"), value: 1, min: 1, max: 1, count: 1 },
      { time: Date.parse("2026-11-01T05:59:00Z"), value: 2, min: 2, max: 2, count: 1 },
      { time: Date.parse("2026-11-01T06:00:00Z"), value: 3, min: 3, max: 3, count: 1 },
      { time: Date.parse("2026-11-01T07:00:00Z"), value: 4, min: 4, max: 4, count: 1 },
    ];
    const model = historyModel(data, [], "America/New_York");
    expect(model.pointOfStop).toEqual([0, 1, -1, 2]);
  });
});

describe("the option", () => {
  test("a number point has the line first, then the band, the hatched gaps and a window over the whole range", () => {
    const data = [...buckets(0, 30), ...buckets(60, 90)];
    const option = historyOption(
      input(data, "number"),
      dashboardFormat("en", "UTC"),
      ENVIRONMENT,
      "Voltage",
      "Voltage chart",
    );
    const series = option.series as { name: string; stack?: string; markArea?: { data: unknown[][] } }[];
    expect(series[0]?.name).toBe("s0");
    expect(series.map((item) => item.name).slice(-2)).toEqual(["band-base", "band-range"]);
    expect(series.filter((item) => item.stack === "band").length).toBe(2);
    // The hole between the runs, and the span from the last bucket to the end of the range.
    expect(series[0]?.markArea?.data.length).toBe(2);
    const xAxis = option.xAxis as { min: number; max: number };
    expect([xAxis.min, xAxis.max]).toEqual([START, START + 2 * HOUR]);
  });

  test("ECharts takes it, draws the hatch and the band, and answers the legend and tooltip actions", () => {
    for (const kind of ["number", "bool"] as const) {
      const data = [
        ...buckets(0, 30, kind === "bool" ? 1 : undefined),
        ...buckets(60, 90, kind === "bool" ? 0 : undefined),
      ];
      const option = historyOption(
        input(data, kind),
        dashboardFormat("en", "UTC"),
        ENVIRONMENT,
        "Voltage",
        "Voltage chart",
      );
      const instance = init(null, null, { renderer: "svg", ssr: true, width: 640, height: 320 });
      try {
        instance.setOption(withSelection(option, [0], new Set()), { notMerge: true });
        instance.dispatchAction({ type: "legendUnSelect", name: "s0" });
        instance.dispatchAction({ type: "legendSelect", name: "s0" });
        instance.dispatchAction({ type: "showTip", seriesIndex: 0, dataIndex: 0 });
        // A zoom given as percentages (what a wheel or a pinch sends) still reports the window as values.
        instance.dispatchAction({ type: "dataZoom", start: 25, end: 50 });
        const zoom = (instance.getOption() as { dataZoom: { startValue: number; endValue: number }[] }).dataZoom[0];
        expect(zoom?.startValue).toBeCloseTo(START + 0.25 * 2 * HOUR, -3);
        expect(zoom?.endValue).toBeCloseTo(START + 0.5 * 2 * HOUR, -3);
        instance.dispatchAction({ type: "dataZoom", start: 0, end: 100 });
        const svg = instance.renderToSVGString();
        // The gap is named in its label.
        expect(svg).toContain("30 minutes without readings");
        if (kind === "bool") expect(svg).toContain("100%");
      } finally {
        instance.dispose();
      }
    }
  });
});
