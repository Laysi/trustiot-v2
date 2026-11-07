import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { type ReactNode, StrictMode } from "react";

import type { Locale } from "../i18n/messages.ts";
import { type FakeChart, fakeEngine } from "../test/fake-chart.ts";
import { type ChartEngine, ChartEngineContext, guardRoot } from "./chart/engine.ts";
import { chartText, seriesName, THRESHOLD_SERIES } from "./chart/options.ts";
import { DashboardEnvProvider } from "./context.tsx";
import { DashboardComponentView } from "./DashboardComponent.tsx";
import type { DashboardData, DatasetResult } from "./data.ts";
import {
  DAILY_ROWS,
  fixtureData,
  GAP_DATASETS,
  GAP_SPECS,
  gapResults,
  POWER_ROWS,
  PROTOTYPE_NAMES,
  PROTOTYPE_SPECS,
  prototypeNameData,
  READINGS_ROWS,
  result,
  SPECS,
  sqlDataset,
  TREND_ROWS,
} from "./fixtures.ts";
import { dashboardFormat, NO_VALUE, wallMs } from "./format.ts";
import { isPlatformComponent, type PlatformComponent, parseComponentSpec } from "./spec.ts";

afterEach(() => {
  cleanup();
});

const LOCALE: Locale = "en";
const format = dashboardFormat(LOCALE, "UTC");

function inEnv(engine: ChartEngine, children: ReactNode, locale: Locale = LOCALE) {
  return (
    <ChartEngineContext.Provider value={engine}>
      <DashboardEnvProvider locale={locale} timeZone="UTC">
        {children}
      </DashboardEnvProvider>
    </ChartEngineContext.Provider>
  );
}

function show(spec: PlatformComponent, data: DashboardData = fixtureData(), locale: Locale = LOCALE) {
  const { engine, charts } = fakeEngine();
  const view = render(inEnv(engine, <DashboardComponentView spec={spec} data={data} />, locale));
  return { ...view, charts };
}

/** Runs `body` with `console.error` silent: React reports there every error a boundary catches. */
async function withQuietErrors(body: () => Promise<void> | void): Promise<void> {
  const original = console.error;
  console.error = () => {};
  try {
    await body();
  } finally {
    console.error = original;
  }
}

function seriesNames(chart: FakeChart): string[] {
  return (chart.option.series as Array<{ name: string }>).map((item) => item.name);
}

async function chartAt(charts: FakeChart[], index = 0): Promise<FakeChart> {
  await waitFor(() => {
    const chart = charts[index];
    if (chart === undefined || chart.options.length === 0)
      throw new Error(`waiting for chart ${index} to get an option`);
  });
  return charts[index] as FakeChart;
}

function tooltipValues(): string[] {
  return within(screen.getByTestId("chart-tooltip"))
    .queryAllByTestId("tip-value")
    .map((element) => element.textContent ?? "");
}

/** The data with one column's declared type replaced, as when a SQL result's type changes. */
function withColumnType(
  data: DashboardData,
  dataset: string,
  column: string,
  type: "number" | "text" | "time" | "bool",
) {
  const state = data.results[dataset];
  if (state?.status !== "ready") throw new Error(`no result for ${dataset}`);
  const result: DatasetResult = {
    ...state.result,
    columns: state.result.columns.map((item) => (item.name === column ? { ...item, type } : item)),
  };
  return { ...data, results: { ...data.results, [dataset]: { status: "ready" as const, result } } };
}

function expectProblem(text: RegExp) {
  expect(screen.getByRole("alert").textContent).toMatch(text);
  expect(screen.queryByTestId("chart-root")).toBeNull();
}

describe("time series", () => {
  test("hands ECharts the rows' values unchanged, one line per series, sorted by time", async () => {
    const { charts } = show(SPECS.timeSeries);
    const chart = await chartAt(charts);
    const series = chart.option.series as Array<{ name: string; data: [number, number | null][] }>;
    const expected = (line: string) =>
      POWER_ROWS.filter((row) => row[1] === line).map((row) => [wallMs(row[0] as string), row[2]]);
    // Named by slot: "Line A" and "Line B" are only in the legend and the tooltip.
    expect(series.map((item) => item.name)).toEqual([seriesName(0), seriesName(1), THRESHOLD_SERIES]);
    expect(series[0]?.data).toEqual(expected("Line A") as never);
    expect(series[1]?.data).toEqual(expected("Line B") as never);
    expect(chart.option.useUTC).toBe(true);
  });

  test("every number in the tooltip is a row's value, formatted", async () => {
    const { charts } = show(SPECS.timeSeries);
    const chart = await chartAt(charts);
    const times = [...new Set(POWER_ROWS.map((row) => row[0] as string))];
    for (const [stop, time] of times.entries()) {
      const rows = POWER_ROWS.filter((row) => row[0] === time);
      // Point at the stop through a series that has it.
      const line = rows[0]?.[1] === "Line A" ? 0 : 1;
      const linePoints = POWER_ROWS.filter((row) => row[1] === (line === 0 ? "Line A" : "Line B"));
      act(() =>
        chart.showTip(
          line,
          linePoints.findIndex((row) => row[0] === time),
        ),
      );
      expect(screen.getByTestId("tip-time").textContent).toBe(format.wallTime(wallMs(time) as number));
      expect(tooltipValues()).toEqual(rows.map((row) => format.withUnit(format.number(row[2] as number | null), "kW")));
      expect(stop).toBeLessThan(times.length);
    }
    // Spot checks against literal text, so the formatter itself is held to something.
    act(() => chart.showTip(0, 0));
    expect(tooltipValues()).toEqual(["12.346 kW", "1,234.5 kW"]);
    act(() => chart.showTip(0, 1));
    expect(tooltipValues()).toEqual(["0.000123 kW", NO_VALUE]);
  });

  test("the threshold line's label is the constant's value, formatted", async () => {
    const { charts } = show(SPECS.timeSeries);
    const chart = await chartAt(charts);
    const helper = (
      chart.option.series as Array<{ markLine?: { data: Array<{ yAxis: number; label: { formatter(): string } }> } }>
    )[2];
    const line = helper?.markLine?.data[0];
    expect(line?.yAxis).toBe(1245.5);
    expect(line?.label.formatter()).toBe("Limit 1,245.5 kW");
  });

  test("a threshold stretches the value axis through invisible points, never by fixing its ends", async () => {
    const data = fixtureData();
    // A threshold far above the data, which the axis would otherwise leave out.
    data.constants = data.constants.map((constant) =>
      constant.name === "limit" ? { ...constant, value: 5000 } : constant,
    );
    const { charts } = show(SPECS.timeSeries, data);
    const chart = await chartAt(charts);
    const series = chart.option.series as Array<{ name: string; data: unknown; tooltip?: { show: boolean } }>;
    const helper = series.find((item) => item.name === THRESHOLD_SERIES);
    expect(helper?.data).toEqual([[wallMs(POWER_ROWS[0]?.[0] as string), 5000]]);
    // Off the tooltip and the axis pointer.
    expect(helper?.tooltip?.show).toBe(false);
    const yAxis = chart.option.yAxis as Record<string, unknown>;
    expect(yAxis.min).toBeUndefined();
    expect(yAxis.max).toBeUndefined();
  });

  test("the keyboard steps through the times, shows the tooltip and reads the values out", async () => {
    const { charts } = show(SPECS.timeSeries);
    const chart = await chartAt(charts);
    const slider = screen.getByRole("slider");
    act(() => slider.focus());
    // Focus selects the newest time, 00:45, where only Line B has a row.
    expect(slider.getAttribute("aria-valuetext")).toBe(
      `${format.wallTime(wallMs("2026-11-01 00:45:00") as number)}, Line B ${format.withUnit(format.number(1250), "kW")}`,
    );
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 2 });
    expect(tooltipValues()).toEqual([format.withUnit("-7.25", "kW"), format.withUnit("1,240.125", "kW")]);
    fireEvent.keyDown(slider, { key: "Home" });
    expect(slider.getAttribute("aria-valuenow")).toBe("0");
    fireEvent.keyDown(slider, { key: "Enter" });
    const panel = await screen.findByRole("region", { name: "Source" });
    expect(within(panel).getByText("SELECT ts, line, kw FROM power ORDER BY ts")).toBeTruthy();
    expect(
      within(panel)
        .getAllByRole("region")
        .map((section) => section.getAttribute("aria-label")),
    ).toEqual(["Row 1", "Row 2"]);
  });

  test("clicking a point opens its rows as the source, every cell formatted", async () => {
    const { charts } = show(SPECS.timeSeries);
    const chart = await chartAt(charts);
    act(() => chart.showTip(1, 2));
    act(() => chart.emitSurface("click"));
    const panel = await screen.findByRole("region", { name: "Source" });
    const cells = within(panel)
      .getAllByTestId("source-cell")
      .map((cell) => cell.textContent);
    const rows = [4, 5].map((row) => POWER_ROWS[row] as DatasetResult["rows"][number]);
    expect(cells).toEqual(
      rows.flatMap((row) => [
        format.cell("time", row[0] ?? null),
        format.cell("text", row[1] ?? null),
        format.cell("number", row[2] ?? null),
      ]),
    );
    expect(within(panel).getByText(format.instant("2026-11-01T02:30:00Z"))).toBeTruthy();
  });

  test("the legend hides a series without replacing the option, and the tooltip leaves it out", async () => {
    const { charts } = show(SPECS.timeSeries);
    const chart = await chartAt(charts);
    const optionsBefore = chart.options.length;
    const lineB = screen.getByRole("button", { name: "Line B" });
    expect(lineB.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(lineB);
    expect(lineB.getAttribute("aria-pressed")).toBe("false");
    expect(chart.actions).toContainEqual({ type: "legendUnSelect", name: seriesName(1) });
    expect(chart.options.length).toBe(optionsBefore);
    act(() => chart.showTip(0, 0));
    expect(tooltipValues()).toEqual([format.withUnit(format.number(12.3456), "kW")]);
  });

  test("dragging across the plot zooms to that range, and the reset button shows the full range", async () => {
    const original = window.matchMedia;
    window.matchMedia = ((query: string) => ({
      matches: query === "(pointer: fine)",
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    })) as unknown as typeof window.matchMedia;
    try {
      const { charts } = show(SPECS.timeSeries);
      const chart = await chartAt(charts);
      expect((chart.option.dataZoom as Array<{ moveOnMouseMove: boolean }>)[0]?.moveOnMouseMove).toBe(false);
      act(() => {
        chart.emitSurface("mousedown", { offsetX: 200, offsetY: 40 });
        chart.emitSurface("mousemove", { offsetX: 120, offsetY: 40 });
      });
      act(() => chart.emitSurface("mouseup", { offsetX: 120, offsetY: 40 }));
      expect(chart.actions).toContainEqual({ type: "dataZoom", startValue: 120, endValue: 200 });
      // The click that ends a drag is not a click on a point.
      act(() => chart.emitSurface("click"));
      expect(screen.queryByRole("region", { name: "Source" })).toBeNull();
      // The renderer drops the click that ends a drag when the pointer moved; the next press and
      // click on a point still count.
      act(() => {
        chart.emitSurface("mousedown", { offsetX: 200, offsetY: 40 });
        chart.emitSurface("mousemove", { offsetX: 120, offsetY: 40 });
      });
      act(() => chart.emitSurface("mouseup", { offsetX: 120, offsetY: 40 }));
      act(() => chart.showTip(1, 2));
      act(() => {
        chart.emitSurface("mousedown", { offsetX: 150, offsetY: 40 });
        chart.emitSurface("mouseup", { offsetX: 150, offsetY: 40 });
        chart.emitSurface("click", { offsetX: 150, offsetY: 40 });
      });
      expect(await screen.findByRole("region", { name: "Source" })).toBeTruthy();
      const reset = screen.getByRole("button", { name: "Show the full range" });
      fireEvent.click(reset);
      expect(chart.actions.at(-1)).toEqual({ type: "dataZoom", start: 0, end: 100 });
      act(() => chart.emit("datazoom", { start: 0, end: 100 }));
      expect(screen.queryByRole("button", { name: "Show the full range" })).toBeNull();
    } finally {
      window.matchMedia = original;
    }
  });

  test("without a fine pointer a finger pans, and its moves are never cancelled", async () => {
    const original = window.matchMedia;
    // A phone: no media query matches, `(pointer: fine)` included.
    window.matchMedia = ((query: string) => ({
      matches: false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    })) as unknown as typeof window.matchMedia;
    try {
      const { charts } = show(SPECS.timeSeries);
      const chart = await chartAt(charts);
      const [zoom] = chart.option.dataZoom as Array<{
        moveOnMouseMove: boolean;
        preventDefaultMouseMove: boolean;
        minValueSpan: number;
      }>;
      expect(zoom?.moveOnMouseMove).toBe(true);
      // A cancelled touchmove would stop the page from scrolling under a vertical swipe.
      expect(zoom?.preventDefaultMouseMove).toBe(false);
      // Zooming in stops at two steps between points (the rows are 15 minutes apart).
      expect(zoom?.minValueSpan).toBe(30 * 60_000);
      expect(screen.getByTestId("chart-root").className).toContain("touch-pan-y");
      expect(screen.getByText("Pinch to zoom, drag to pan, tap a point to see its source.")).toBeTruthy();
    } finally {
      window.matchMedia = original;
    }
  });

  test("a y column that is no longer a number shows an error with its source, and draws nothing", async () => {
    show(SPECS.timeSeries, withColumnType(fixtureData(), "power", "kw", "text"));
    expectProblem(/needs a number column/);
    fireEvent.click(screen.getByRole("button", { name: "See the source: Line power" }));
    expect(await screen.findByText("The whole result, 7 rows")).toBeTruthy();
  });

  test("an x column that is not a time is an error", () => {
    show(SPECS.timeSeries, withColumnType(fixtureData(), "power", "ts", "text"));
    expectProblem(/needs a time column/);
  });

  test("two rows for one series and time are an error, never summed", () => {
    show({ ...SPECS.timeSeries, series: undefined });
    expectProblem(/two values for one point of this chart/);
  });

  test("a dataset still loading shows a placeholder; a failed one says why", () => {
    const data = fixtureData();
    const loading = show(SPECS.timeSeries, { ...data, results: { ...data.results, power: { status: "loading" } } });
    expect(screen.getByRole("status").textContent).toContain("Loading data…");
    loading.unmount();
    show(SPECS.timeSeries, {
      ...data,
      results: { ...data.results, power: { status: "failed", message: "Timed out." } },
    });
    expectProblem(/The query of dataset “Line power” failed: Timed out\./);
  });

  test("a result without rows says so and offers the source", async () => {
    const data = fixtureData();
    const state = data.results.power;
    if (state?.status !== "ready") throw new Error("fixture");
    show(SPECS.timeSeries, {
      ...data,
      results: { ...data.results, power: { status: "ready", result: { ...state.result, rows: [], row_count: 0 } } },
    });
    expect(screen.getByRole("status").textContent).toContain("This query returned no rows.");
    expect(screen.queryByTestId("chart-root")).toBeNull();
  });
});

describe("data gaps", () => {
  const at = (text: string) => wallMs(text) as number;
  const hours = (from: number, to: number) =>
    Array.from({ length: to - from + 1 }, (_, index) => `2026-11-01 ${String(from + index).padStart(2, "0")}:00:00`);
  /** Line A reports hourly from 00:00 to 05:00 and from 15:00 to 16:00: ten hours without rows between. */
  const A_TIMES = [...hours(0, 5), ...hours(15, 16)];
  const A_BREAK = at("2026-11-01 10:00:00");
  const single = {
    kind: "time_series",
    title: "Power",
    dataset: "power",
    x: "ts",
    y: ["kw"],
    unit: "kW",
  } satisfies PlatformComponent;

  function withPower(rows: DatasetResult["rows"]): DashboardData {
    const data = fixtureData();
    return {
      ...data,
      results: {
        ...data.results,
        power: {
          status: "ready",
          result: result(
            "power",
            [
              { name: "ts", type: "time" },
              { name: "line", type: "text" },
              { name: "kw", type: "number" },
            ],
            rows,
          ),
        },
      },
    };
  }

  const gappedRows = (): DatasetResult["rows"] => A_TIMES.map((time, index) => [time, "Line A", 10 + index]);

  type Area = Array<[{ xAxis: number; label: { show: boolean; formatter(): string } }, { xAxis: number }]>;
  type Line = { data: Array<[number, number | null]>; markArea?: { data: Area }; sampling?: string };

  test("breaks the line and hatches the span where a series has no rows for longer than three times its spacing", async () => {
    const { charts } = show(single, withPower(gappedRows()));
    const chart = await chartAt(charts);
    const line = (chart.option.series as Line[])[0] as Line;
    expect(line.data.map(([x]) => x)).toEqual([...A_TIMES.slice(0, 6).map(at), A_BREAK, ...A_TIMES.slice(6).map(at)]);
    expect(line.data.map(([, y]) => y)).toEqual([10, 11, 12, 13, 14, 15, null, 16, 17]);
    // Thinning the line could drop the break.
    expect(line.sampling).toBeUndefined();
    const area = (line.markArea as { data: Area }).data;
    expect(area.length).toBe(1);
    expect(area[0]?.[0].xAxis).toBe(at("2026-11-01 05:00:00"));
    expect(area[0]?.[1].xAxis).toBe(at("2026-11-01 15:00:00"));
    // Ten of the sixteen hours: wide enough to be named, in words that fit any dataset.
    expect(area[0]?.[0].label.show).toBe(true);
    expect(area[0]?.[0].label.formatter()).toBe("10 hours without data");
    // The axis still runs from the first to the last row: no gap is invented at an end.
    const xAxis = chart.option.xAxis as { min: number; max: number };
    expect([xAxis.min, xAxis.max]).toEqual([at(A_TIMES[0] as string), at(A_TIMES.at(-1) as string)]);
  });

  test("hatches every gap but names only the longest one", async () => {
    // Rows 00:00-05:00, 12:00-17:00 and 30:00-35:00 (hours from the first midnight): holes of 7 and 13 hours.
    const stamp = (hour: number) =>
      new Date(Date.UTC(2026, 10, 1) + hour * 3_600_000).toISOString().slice(0, 19).replace("T", " ");
    const runs = [...Array(6).keys()].flatMap((hour) => [hour, hour + 12, hour + 30]).sort((a, b) => a - b);
    const { charts } = show(single, withPower(runs.map((hour) => [stamp(hour), "Line A", hour])));
    const chart = await chartAt(charts);
    const area = ((chart.option.series as Line[])[0] as Line).markArea?.data ?? [];
    expect(area.length).toBe(2);
    expect(area.map((item) => item[0].label.show)).toEqual([false, true]);
    expect(area[1]?.[0].label.formatter()).toBe("13 hours without data");
  });

  test("names the gap in the viewer's language", async () => {
    const { charts } = show(single, withPower(gappedRows()), "zh-TW");
    const chart = await chartAt(charts);
    const line = (chart.option.series as Line[])[0] as Line;
    expect(line.markArea?.data[0]?.[0].label.formatter()).toBe("10 小時沒有資料");
  });

  test("keeps a line whole while no hole is longer than three times the spacing", async () => {
    const times = [...hours(0, 4), ...hours(7, 8)];
    const { charts } = show(single, withPower(times.map((time, index) => [time, "Line A", index])));
    const chart = await chartAt(charts);
    const line = (chart.option.series as Line[])[0] as Line;
    expect(line.data).toEqual(times.map((time, index) => [at(time), index]));
    expect(line.markArea).toBeUndefined();
    expect(line.sampling).toBe("lttb");
  });

  test("draws one or two rows as they are", async () => {
    for (const rows of [
      [["2026-11-01 00:00:00", "Line A", 5]],
      [
        ["2026-11-01 00:00:00", "Line A", 5],
        ["2026-11-20 00:00:00", "Line A", 6],
      ],
    ] satisfies DatasetResult["rows"][]) {
      const { charts } = show(single, withPower(rows));
      const chart = await chartAt(charts);
      const line = (chart.option.series as Line[])[0] as Line;
      expect(line.data).toEqual(rows.map((row) => [at(row[0] as string), row[2]]) as never);
      expect(line.markArea).toBeUndefined();
      cleanup();
    }
  });

  test("each series has its own gaps", async () => {
    const rows: DatasetResult["rows"] = [
      ...gappedRows(),
      ...hours(0, 16).map((time, index): DatasetResult["rows"][number] => [time, "Line B", 100 + index]),
    ];
    const { charts } = show({ ...single, series: "line" }, withPower(rows));
    const chart = await chartAt(charts);
    const [a, b] = chart.option.series as Line[];
    expect(a?.markArea?.data.length).toBe(1);
    expect(a?.data.some(([, y]) => y === null)).toBe(true);
    expect(b?.markArea).toBeUndefined();
    expect(b?.data.length).toBe(17);
  });

  test("the keyboard steps over the rows only, and the tooltip and source never name a break", async () => {
    const { charts } = show(single, withPower(gappedRows()));
    const chart = await chartAt(charts);
    const slider = screen.getByRole("slider");
    // Eight rows, eight stops: the break is not one of them.
    expect(slider.getAttribute("aria-valuemax")).toBe("7");
    act(() => slider.focus());
    fireEvent.keyDown(slider, { key: "Home" });
    for (let step = 0; step < 5; step++) fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(screen.getByTestId("tip-time").textContent).toBe(format.wallTime(at("2026-11-01 05:00:00")));
    // From the last row before the gap to the first after it, past the break's place in the data.
    fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 7 });
    expect(screen.getByTestId("tip-time").textContent).toBe(format.wallTime(at("2026-11-01 15:00:00")));
    expect(tooltipValues()).toEqual([format.withUnit("16", "kW")]);
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 5 });
    // Pointing at the break itself shows nothing new.
    act(() => chart.showTip(0, 6));
    expect(screen.getByTestId("tip-time").textContent).toBe(format.wallTime(at("2026-11-01 05:00:00")));
    // A point after the gap is the row behind that data index.
    act(() => chart.showTip(0, 8));
    expect(tooltipValues()).toEqual([format.withUnit("17", "kW")]);
    act(() => chart.emitSurface("click"));
    const panel = await screen.findByRole("region", { name: "Source" });
    expect(
      within(panel)
        .getAllByRole("region")
        .map((section) => section.getAttribute("aria-label")),
    ).toEqual(["Row 8"]);
  });

  test("the UI preview's week of a gateway that is off at night breaks at the two nights and not at the short hole", async () => {
    const base = fixtureData();
    const data: DashboardData = {
      ...base,
      datasets: [...base.datasets, ...GAP_DATASETS],
      results: {
        ...base.results,
        climate: { status: "ready", result: gapResults().get("climate") as DatasetResult },
      },
    };
    const climate = show(GAP_SPECS.timeSeries, data);
    const chart = await chartAt(climate.charts);
    const lines = chart.option.series as Line[];
    // Temperature and humidity share their times, so both lines break at the same two places.
    for (const line of lines.slice(0, 2)) {
      expect(line.data.filter(([, y]) => y === null).length).toBe(2);
      expect(line.markArea?.data.length).toBe(2);
    }
    // Both nights are hatched, one label names the longest (the first of two equal ones): two
    // labels are wider than the nights between them.
    const named = lines.slice(0, 2).flatMap((line) => (line.markArea?.data ?? []).filter((area) => area[0].label.show));
    expect(named.map((area) => area[0].label.formatter())).toEqual(["17 hours without data"]);
    expect(lines[0]?.markArea?.data[0]?.[0].label.show).toBe(true);
    cleanup();
    const kpi = show(GAP_SPECS.kpi, data);
    const trend = (await chartAt(kpi.charts)).option.series as Line[];
    expect(trend[0]?.data.filter(([, y]) => y === null).length).toBe(2);
    expect(trend[0]?.markArea).toBeUndefined();
  });

  test("the KPI's trend breaks its line at a gap and draws no hatching", async () => {
    const days = ["2026-10-20", "2026-10-21", "2026-10-22", "2026-10-23", "2026-10-31", "2026-11-01"];
    const rows: DatasetResult["rows"] = days.map((day, index) => [day, 300 + index]);
    const data = fixtureData();
    const withTrend: DashboardData = {
      ...data,
      results: {
        ...data.results,
        trend: {
          status: "ready",
          result: result(
            "trend",
            [
              { name: "day", type: "time" },
              { name: "kwh", type: "number" },
            ],
            rows,
          ),
        },
      },
    };
    const { charts } = show(SPECS.kpi, withTrend);
    const chart = await chartAt(charts);
    const [line, ...others] = chart.option.series as Line[];
    expect(line?.data.map(([, y]) => y)).toEqual([300, 301, 302, 303, null, 304, 305]);
    expect(line?.data[4]?.[0]).toBe((at("2026-10-23") + at("2026-10-31")) / 2);
    expect(line?.markArea).toBeUndefined();
    expect(others.some((other) => other.markArea !== undefined)).toBe(false);
    // The tooltip reads each row at its place in the data, a break has none.
    act(() => chart.showTip(0, 5));
    expect(tooltipValues()).toEqual([format.withUnit("304", "kWh")]);
    act(() => chart.showTip(0, 6));
    expect(tooltipValues()).toEqual([format.withUnit("305", "kWh")]);
    act(() => chart.showTip(0, 4));
    expect(tooltipValues()).toEqual([format.withUnit("305", "kWh")]);
    const slider = screen.getByRole("slider");
    expect(slider.getAttribute("aria-valuemax")).toBe("5");
    act(() => slider.focus());
    fireEvent.keyDown(slider, { key: "Home" });
    for (let step = 0; step < 3; step++) fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 3 });
    fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(chart.actions.at(-1)).toEqual({ type: "showTip", seriesIndex: 0, dataIndex: 5 });
  });
});

describe("motion", () => {
  async function animationWith(reduce: boolean): Promise<unknown> {
    const original = window.matchMedia;
    window.matchMedia = ((query: string) => ({
      matches: reduce && query === "(prefers-reduced-motion: reduce)",
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    })) as unknown as typeof window.matchMedia;
    try {
      const { charts, unmount } = show(SPECS.bar);
      const chart = await chartAt(charts);
      const animation = chart.option.animation;
      unmount();
      return animation;
    } finally {
      window.matchMedia = original;
    }
  }

  test("charts animate, except under prefers-reduced-motion: reduce", async () => {
    expect(await animationWith(false)).toBe(true);
    expect(await animationWith(true)).toBe(false);
  });
});

describe("bar", () => {
  test("hands ECharts each row's value in its category, and every tooltip number is that value", async () => {
    const { charts } = show(SPECS.bar);
    const chart = await chartAt(charts);
    const series = chart.option.series as Array<{ name: string; data: (number | null)[] }>;
    expect(series.map((item) => item.name)).toEqual([seriesName(0), seriesName(1)]);
    expect(series[0]?.data).toEqual([120.5, 133.333, 0.5]);
    expect(series[1]?.data).toEqual([98.25, 101, null]);
    // Categories by index; the axis draws each category's label through the formatter.
    const xAxis = chart.option.xAxis as { data: string[]; axisLabel: { formatter(value: string): string } };
    expect(xAxis.data).toEqual(["0", "1", "2"]);
    expect(xAxis.data.map((value) => xAxis.axisLabel.formatter(value))).toEqual(["Mon", "Tue", "Wed"]);
    for (const [stop, day] of ["Mon", "Tue", "Wed"].entries()) {
      act(() => chart.showTip(0, stop));
      expect(screen.getByTestId("tip-time").textContent).toBe(day);
      expect(tooltipValues()).toEqual(
        DAILY_ROWS.filter((row) => row[0] === day).map((row) =>
          format.withUnit(format.number(row[2] as number), "kWh"),
        ),
      );
    }
  });

  test("clicking a bar group opens its rows", async () => {
    const { charts } = show(SPECS.bar);
    const chart = await chartAt(charts);
    act(() => chart.showTip(1, 1));
    act(() => chart.emitSurface("click"));
    const panel = await screen.findByRole("region", { name: "Source" });
    expect(
      within(panel)
        .getAllByRole("region")
        .map((section) => section.getAttribute("aria-label")),
    ).toEqual(["Row 3", "Row 4"]);
  });

  test("a group with a bar for every series lists each of their rows as the source", async () => {
    const data = fixtureData();
    const state = data.results.daily;
    if (state?.status !== "ready") throw new Error("fixture");
    const rows = Array.from({ length: 6 }, (_, index) => ["Mon", `Site ${index + 1}`, index + 0.5]);
    const { charts } = show(SPECS.bar, {
      ...data,
      results: { ...data.results, daily: { status: "ready", result: { ...state.result, rows, row_count: 6 } } },
    });
    const chart = await chartAt(charts);
    act(() => chart.showTip(0, 0));
    act(() => chart.emitSurface("click"));
    const panel = await screen.findByRole("region", { name: "Source" });
    expect(
      within(panel)
        .getAllByRole("region")
        .map((section) => section.getAttribute("aria-label")),
    ).toEqual(["Row 1", "Row 2", "Row 3", "Row 4", "Row 5", "Row 6"]);
    expect(
      within(panel)
        .getAllByTestId("source-cell")
        .map((cell) => cell.textContent),
    ).toEqual(rows.flatMap((row) => [String(row[0]), String(row[1]), format.number(row[2] as number)]));
    expect(within(panel).queryByText(/more rows/)).toBeNull();
  });

  test("a y column that is not a number is an error", () => {
    show(SPECS.bar, withColumnType(fixtureData(), "daily", "kwh", "bool"));
    expectProblem(/needs a number column/);
  });

  test("more series than colors is an error, not a reused color", () => {
    const data = fixtureData();
    const state = data.results.daily;
    if (state?.status !== "ready") throw new Error("fixture");
    const rows = Array.from({ length: 7 }, (_, index) => ["Mon", `Site ${index + 1}`, index]);
    show(SPECS.bar, {
      ...data,
      results: { ...data.results, daily: { status: "ready", result: { ...state.result, rows, row_count: 7 } } },
    });
    expectProblem(/would draw more than 6 series/);
  });
});

describe("KPI", () => {
  test("shows the computation's value and the comparison, each the formatted result", async () => {
    const { charts } = show(SPECS.kpi);
    const total = DAILY_ROWS.reduce((sum, row) => sum + (row[2] as number), 0);
    expect(screen.getByTestId("value").textContent).toBe(format.number(total));
    expect(screen.getByTestId("value").textContent).toBe("453.583");
    expect(screen.getByTestId("comparison").textContent).toBe(format.withUnit(format.number(98.25 * 2), "kWh"));
    fireEvent.click(screen.getByRole("button", { name: /453\.583/ }));
    const panel = await screen.findByRole("region", { name: "Source" });
    expect(within(panel).getByText("sum(daily.kwh)")).toBeTruthy();
    // The trend: every tooltip number is a row of its dataset.
    const chart = await chartAt(charts);
    for (const [index, row] of TREND_ROWS.entries()) {
      act(() => chart.showTip(0, index));
      expect(tooltipValues()).toEqual([format.withUnit(format.number(row[1] as number | null), "kWh")]);
      expect(screen.getByTestId("tip-time").textContent).toBe(
        format.wallTime(wallMs(row[0] as string) as number, true),
      );
    }
    // The sparkline hides its dots, so the last value, with no value before it, gets a dot of
    // its own under the same series name: the legend and the tooltip treat both as one.
    const series = chart.option.series as Array<{ name: string; data: unknown; tooltip?: { show: boolean } }>;
    expect(series.map((item) => item.name)).toEqual([seriesName(0), seriesName(0)]);
    expect(series[1]?.data).toEqual([[wallMs("2026-10-30") as number, 321.5]]);
    expect(series[1]?.tooltip?.show).toBe(false);
  });

  test("a cell value that is not a number is an error", () => {
    show({ ...SPECS.kpi, value: { kind: "cell", dataset: "readings", column: "meter", row: 0 } });
    expectProblem(/needs a number column/);
  });

  test("a computation over a column whose type changed is an error, not a guess", () => {
    show(SPECS.kpi, withColumnType(fixtureData(), "daily", "kwh", "text"));
    expectProblem(/The formula of computation “Total energy” can't be evaluated on this result\./);
  });
});

describe("table", () => {
  test("every cell is the row's value formatted by its column's type", () => {
    show(SPECS.table);
    const cells = screen.getAllByTestId("table-cell").map((cell) => cell.textContent);
    const types = ["text", "number", "bool", "time", "time"] as const;
    expect(cells).toEqual(
      READINGS_ROWS.flatMap((row) => types.map((type, column) => format.cell(type, row[column] ?? null))),
    );
    expect(cells.slice(0, 5)).toEqual(["Meter 1", "42.123", "Yes", "Oct 31, 2026", "Nov 1, 2026, 1:02:03 AM"]);
    expect(cells.slice(5, 10)).toEqual(["Meter 2", NO_VALUE, "No", "Oct 30, 2026", "Nov 1, 2026, 1:05 AM"]);
    expect(screen.getByRole("columnheader", { name: "value (kWh)" })).toBeTruthy();
  });

  test("clicking a cell opens its row", async () => {
    show(SPECS.table);
    fireEvent.click(screen.getAllByTestId("table-cell")[7] as HTMLElement);
    const panel = await screen.findByRole("region", { name: "Source" });
    expect(within(panel).getByRole("region", { name: "Row 2" })).toBeTruthy();
  });

  test("a column the result no longer has is an error", () => {
    show({ ...SPECS.table, columns: [{ column: "meter" }, { column: "voltage" }] });
    expectProblem(/The data has no column this item uses/);
  });
});

describe("status", () => {
  test("shows the value, the band in words and the thresholds, each formatted", () => {
    show(SPECS.status);
    expect(screen.getByTestId("value").textContent).toBe(format.number(42.1234));
    expect(screen.getByTestId("status-band").textContent).toBe(`Below Warning ${format.withUnit("100", "kW")}`);
    expect(screen.getAllByTestId("threshold").map((item) => item.textContent)).toEqual([
      `Warning ${format.withUnit("100", "kW")}`,
      `Limit ${format.withUnit("1,245.5", "kW")}`,
    ]);
  });

  test("a value between two thresholds names both", () => {
    show({
      ...SPECS.status,
      value: { kind: "cell", dataset: "readings", column: "value", row: 2 },
      thresholds: ["warn"],
    });
    expect(screen.getByTestId("status-band").textContent).toBe(`At or above Warning ${format.withUnit("100", "kW")}`);
    cleanup();
    const data = fixtureData();
    show({ ...SPECS.status, value: { kind: "computation", name: "total_kwh" } }, data);
    expect(screen.getByTestId("status-band").textContent).toBe(
      `Between Warning ${format.withUnit("100", "kW")} and Limit ${format.withUnit("1,245.5", "kW")}`,
    );
  });

  test("thresholds out of order are an error", () => {
    show({ ...SPECS.status, thresholds: ["limit", "warn"] });
    expectProblem(/Thresholds must go from low to high/);
  });

  test("a value column that is not a number is an error", () => {
    show(SPECS.status, withColumnType(fixtureData(), "readings", "value", "text"));
    expectProblem(/needs a number column/);
  });
});

describe("zh-TW", () => {
  test("numbers and messages follow the language", () => {
    const zh = dashboardFormat("zh-TW", "UTC");
    show(SPECS.status, fixtureData(), "zh-TW");
    expect(screen.getByTestId("status-band").textContent).toBe(`低於Warning ${zh.withUnit(zh.number(100), "kW")}`);
    cleanup();
    show(SPECS.status, withColumnType(fixtureData(), "readings", "value", "bool"), "zh-TW");
    expect(screen.getByRole("alert").textContent).toContain(
      "這個項目需要數值欄位，但資料的欄位是其他型別，請 AI 助理更新儀表板。",
    );
    cleanup();
    show(SPECS.table, fixtureData(), "zh-TW");
    expect(screen.getAllByTestId("table-cell")[1]?.textContent).toBe(zh.number(42.1234));
    expect(screen.getAllByTestId("table-cell")[2]?.textContent).toBe("是");
  });
});

describe("spec", () => {
  test("refuses unknown kinds and fields, colors and styles included", () => {
    expect(parseComponentSpec(SPECS.timeSeries)).not.toBeNull();
    expect(parseComponentSpec({ ...SPECS.timeSeries, color: "#ff0000" })).toBeNull();
    expect(parseComponentSpec({ ...SPECS.bar, style: { fill: "red" } })).toBeNull();
    expect(parseComponentSpec({ ...SPECS.table, kind: "pie" })).toBeNull();
    // The contract has no field that picks the mark: a time series is lines, never an area.
    expect(parseComponentSpec({ ...SPECS.timeSeries, mark: "area" })).toBeNull();
    expect(
      parseComponentSpec({
        ...SPECS.kpi,
        value: { kind: "cell", dataset: "daily", column: "kwh", row: 0, html: "<b>" },
      }),
    ).toBeNull();
  });

  test("counts a title's and a unit's length in characters, as the contract does", () => {
    // U+1F321 is one character but two UTF-16 units.
    const title = "\u{1F321}".repeat(120);
    expect(parseComponentSpec({ ...SPECS.timeSeries, title })).not.toBeNull();
    expect(parseComponentSpec({ ...SPECS.timeSeries, title: `${title}x` })).toBeNull();
    expect(parseComponentSpec({ ...SPECS.timeSeries, title: "" })).toBeNull();
    const unit = "\u{1F321}".repeat(24);
    expect(parseComponentSpec({ ...SPECS.bar, unit })).not.toBeNull();
    expect(parseComponentSpec({ ...SPECS.bar, unit: `${unit}x` })).toBeNull();
  });
});

/** `data` with one more dataset, declared with `maxRows` and holding `rows`. */
function withDataset(
  data: DashboardData,
  name: string,
  columns: DatasetResult["columns"],
  rows: DatasetResult["rows"],
  maxRows = 2000,
): DashboardData {
  return {
    ...data,
    datasets: [
      ...data.datasets,
      { ...sqlDataset(name, `Dataset ${name}`, `SELECT 1 FROM ${name}`), max_rows: maxRows },
    ],
    results: { ...data.results, [name]: { status: "ready", result: result(name, columns, rows) } },
  };
}

/** The legend's buttons, in order. */
function legendNames(): string[] {
  return screen
    .getAllByRole("button")
    .filter((button) => button.hasAttribute("aria-pressed"))
    .map((button) => button.textContent ?? "");
}

const TARIFF_COLUMNS: DatasetResult["columns"] = [
  { name: "day", type: "text" },
  { name: "site", type: "text" },
  { name: "peak", type: "number" },
  { name: "offpeak", type: "number" },
];

function tariffRows(sites: number): DatasetResult["rows"] {
  return ["Mon", "Tue"].flatMap((day, dayIndex) =>
    Array.from({ length: sites }, (_, site): DatasetResult["rows"][number] => [
      day,
      `Site ${site + 1}`,
      dayIndex * 10 + site + 0.5,
      dayIndex * 10 + site + 0.25,
    ]),
  );
}

// The contract's `DashboardComponent` (Laysi/trustiot-v2#987): what it allows that the stand-in
// did not, and what the stand-in allowed that it does not.
describe("the contract's component list", () => {
  test("reads the shapes the contract writes, and refuses the stand-in's", () => {
    const cell = { kind: "cell", dataset: "readings", column: "value", row: 0 };
    expect(parseComponentSpec({ ...SPECS.kpi, value: cell })).not.toBeNull();
    expect(parseComponentSpec({ ...SPECS.kpi, value: { kind: "computation", name: "total_kwh" } })).not.toBeNull();
    // A value without `kind`, `computation` as a key and `comparison` were the stand-in's.
    expect(parseComponentSpec({ ...SPECS.kpi, value: { dataset: "readings", column: "value", row: 0 } })).toBeNull();
    expect(parseComponentSpec({ ...SPECS.kpi, value: { computation: "total_kwh" } })).toBeNull();
    expect(parseComponentSpec({ ...SPECS.kpi, value: { kind: "computation", name: "total_kwh", row: 0 } })).toBeNull();
    expect(parseComponentSpec({ ...SPECS.kpi, comparison: "last_week" })).toBeNull();
    expect(parseComponentSpec({ ...SPECS.bar, y: "kwh" })).toBeNull();
  });

  test("a row is bounded by its dataset's max_rows, not by a number of the component's own", () => {
    const cell = (row: number) => ({ kind: "cell" as const, dataset: "readings", column: "value", row });
    expect(parseComponentSpec({ ...SPECS.status, value: cell(2000) })).not.toBeNull();
    expect(parseComponentSpec({ ...SPECS.status, value: cell(-1) })).toBeNull();
    // Below max_rows and in the result: shown. Below max_rows but not in the result: this run
    // did not return it. At or past max_rows: the spec itself is wrong, whatever the result is.
    const data = fixtureData();
    show({ ...SPECS.status, value: cell(2) }, data);
    expect(screen.getByTestId("value").textContent).toBe(format.number(1000000.5));
    cleanup();
    show({ ...SPECS.status, value: cell(3) }, data);
    expectProblem(/did not return this time/);
    cleanup();
    const small = {
      ...data,
      datasets: data.datasets.map((dataset) => (dataset.name === "readings" ? { ...dataset, max_rows: 2 } : dataset)),
    };
    show({ ...SPECS.status, value: cell(2) }, small);
    expectProblem(/past the data source's row limit/);
    cleanup();
    show({ ...SPECS.status, value: cell(1) }, small);
    expect(screen.getByTestId("value").textContent).toBe(NO_VALUE);
  });

  test("a KPI's comparison is `compare`, shown with its own computation's unit", () => {
    show({ ...SPECS.kpi, unit: "MWh", compare: "total_kwh" });
    expect(screen.getByTestId("value").textContent).toContain(format.number(453.583));
    // The component's unit is the value's; the comparison keeps the computation's.
    expect(screen.getByTestId("comparison").textContent).toBe(format.withUnit(format.number(453.583), "kWh"));
    cleanup();
    show({ ...SPECS.kpi, compare: null });
    expect(screen.queryByTestId("comparison")).toBeNull();
  });

  test("null is the same as absent for every optional field", () => {
    const nulls = {
      ...SPECS.kpi,
      compare: null,
      trend: null,
      unit: null,
    };
    expect(parseComponentSpec(nulls)).not.toBeNull();
    show(nulls);
    expect(screen.getByTestId("value").textContent).toBe("453.583");
    expect(screen.queryByTestId("comparison")).toBeNull();
    expect(screen.queryByTestId("chart-root")).toBeNull();
    cleanup();
    show({ ...SPECS.status, unit: null });
    expect(screen.getByTestId("value").textContent).toBe(format.number(42.1234));
    cleanup();
    show({ ...SPECS.table, columns: [{ column: "meter", unit: null }, { column: "value" }] });
    expect(screen.getByRole("columnheader", { name: "meter" })).toBeTruthy();
    cleanup();
    // `series: null` splits nothing, so these rows have two values for one point.
    const flat = { ...SPECS.timeSeries, series: null, unit: null };
    expect(parseComponentSpec(flat)).not.toBeNull();
    show(flat);
    expectProblem(/two values for one point of this chart/);
    expect(screen.queryByText(/^Unit:/)).toBeNull();
  });

  test("a table has at most 16 columns", () => {
    const columns = (count: number) => Array.from({ length: count }, (_, index) => ({ column: `c${index}` }));
    expect(parseComponentSpec({ ...SPECS.table, columns: columns(16) })).not.toBeNull();
    expect(parseComponentSpec({ ...SPECS.table, columns: columns(17) })).toBeNull();
    expect(parseComponentSpec({ ...SPECS.table, columns: [] })).toBeNull();
  });

  test("the custom page is an item of the list that no component draws", () => {
    const item = parseComponentSpec({ kind: "custom_page" });
    expect(item).toEqual({ kind: "custom_page" });
    expect(parseComponentSpec({ kind: "custom_page", title: "Page" })).toBeNull();
    const list = [SPECS.table, { kind: "custom_page" as const }, SPECS.status];
    expect(list.filter(isPlatformComponent).map((spec) => spec.kind)).toEqual(["table", "status"]);
    expect(isPlatformComponent({ kind: "custom_page" })).toBe(false);
  });

  describe("charts with several y columns", () => {
    const columns = TARIFF_COLUMNS;

    test("a bar chart without a series draws a bar in each group for each y", async () => {
      const data = withDataset(fixtureData(), "tariff", columns, tariffRows(1));
      const { charts } = show(
        { kind: "bar", title: "Tariff", dataset: "tariff", x: "day", y: ["peak", "offpeak"] },
        data,
      );
      const chart = await chartAt(charts);
      const series = chart.option.series as Array<{ name: string; data: (number | null)[] }>;
      expect(series.map((item) => item.name)).toEqual([seriesName(0), seriesName(1)]);
      expect(series[0]?.data).toEqual([0.5, 10.5]);
      expect(series[1]?.data).toEqual([0.25, 10.25]);
      expect(screen.getByRole("button", { name: "peak" })).toBeTruthy();
      expect(screen.getByRole("button", { name: "offpeak" })).toBeTruthy();
    });

    test("with a series, there is a bar for each value of it and each y, named by both", async () => {
      const data = withDataset(fixtureData(), "tariff", columns, tariffRows(2));
      const { charts } = show(
        { kind: "bar", title: "Tariff", dataset: "tariff", x: "day", y: ["peak", "offpeak"], series: "site" },
        data,
      );
      const chart = await chartAt(charts);
      const series = chart.option.series as Array<{ name: string; data: (number | null)[] }>;
      expect(series.map((item) => item.data)).toEqual([
        [0.5, 10.5],
        [0.25, 10.25],
        [1.5, 11.5],
        [1.25, 11.25],
      ]);
      expect(legendNames()).toEqual(["Site 1 · peak", "Site 1 · offpeak", "Site 2 · peak", "Site 2 · offpeak"]);
      // A tooltip row is a row's value, in the series' own slot.
      act(() => chart.showTip(1, 1));
      expect(tooltipValues()).toContain(format.withUnit(format.number(10.25), undefined));
    });

    test("a time series with a series and several y draws a line for each pair", async () => {
      const data = withDataset(
        fixtureData(),
        "phases",
        [
          { name: "ts", type: "time" },
          { name: "phase", type: "text" },
          { name: "amps", type: "number" },
          { name: "volts", type: "number" },
        ],
        [
          ["2026-11-01 00:00:00", "A", 1.5, 230.5],
          ["2026-11-01 00:00:00", "B", 2.5, 231.5],
          ["2026-11-01 00:15:00", "A", 1.75, 229.5],
        ],
      );
      const { charts } = show(
        { kind: "time_series", title: "Phases", dataset: "phases", x: "ts", y: ["amps", "volts"], series: "phase" },
        data,
      );
      const chart = await chartAt(charts);
      const series = chart.option.series as Array<{ data: unknown }>;
      expect(series.slice(0, 4).map((item) => (item.data as unknown[][]).map((point) => point[1]))).toEqual([
        [1.5, 1.75],
        [230.5, 229.5],
        [2.5],
        [231.5],
      ]);
      expect(legendNames()).toEqual(["A · amps", "A · volts", "B · amps", "B · volts"]);
    });

    test("a series value that holds the label separator keeps its own legend entry for each y", async () => {
      // "A · amps" is a series value here, and also the label of series A with the y column amps:
      // each pair hides on its own, whatever the values read like.
      const data = withDataset(
        fixtureData(),
        "phases",
        [
          { name: "ts", type: "time" },
          { name: "phase", type: "text" },
          { name: "amps", type: "number" },
          { name: "volts", type: "number" },
        ],
        [
          ["2026-11-01 00:00:00", "A", 1.5, 230.5],
          ["2026-11-01 00:00:00", "A · amps", 2.5, 231.5],
        ],
      );
      const { charts } = show(
        { kind: "time_series", title: "Phases", dataset: "phases", x: "ts", y: ["amps", "volts"], series: "phase" },
        data,
      );
      const chart = await chartAt(charts);
      fireEvent.click(
        screen.getAllByRole("button").filter((button) => button.hasAttribute("aria-pressed"))[0] as HTMLElement,
      );
      expect(chart.actions.filter((action) => action.type === "legendUnSelect")).toEqual([
        { type: "legendUnSelect", name: seriesName(0) },
      ]);
      expect(legendNames().length).toBe(4);
      expect(
        screen
          .getAllByRole("button")
          .filter((button) => button.hasAttribute("aria-pressed"))
          .map((button) => button.getAttribute("aria-pressed")),
      ).toEqual(["false", "true", "true", "true"]);
    });

    test("y columns times the series' values are at most 6, as the contract counts", () => {
      const spec: PlatformComponent = {
        kind: "bar",
        title: "Tariff",
        dataset: "tariff",
        x: "day",
        y: ["peak", "offpeak"],
        series: "site",
      };
      show(spec, withDataset(fixtureData(), "tariff", columns, tariffRows(3)));
      expect(screen.queryByRole("alert")).toBeNull();
      cleanup();
      show(spec, withDataset(fixtureData(), "tariff", columns, tariffRows(4)));
      expectProblem(/would draw more than 6 series/);
    });

    test("at most 6 y columns", () => {
      const y = (count: number) => Array.from({ length: count }, (_, index) => `c${index}`);
      expect(parseComponentSpec({ ...SPECS.bar, y: y(6) })).not.toBeNull();
      expect(parseComponentSpec({ ...SPECS.bar, y: y(7) })).toBeNull();
      expect(parseComponentSpec({ ...SPECS.timeSeries, y: [] })).toBeNull();
    });
  });
});

describe("chart root", () => {
  test("clears through replaceChildren and refuses any HTML", () => {
    const root = document.createElement("div");
    root.append(document.createElement("canvas"));
    guardRoot(root);
    root.innerHTML = "";
    expect(root.childElementCount).toBe(0);
    expect(() => {
      root.innerHTML = "<img src=x>";
    }).toThrow("the chart root accepts no HTML");
    expect(root.childElementCount).toBe(0);
  });

  test("the chart is disposed when the component unmounts", async () => {
    const { charts, unmount } = show(SPECS.bar);
    const chart = await chartAt(charts);
    unmount();
    expect(chart.disposed).toBe(true);
  });

  test("mounted twice in a row, as StrictMode does, the second mount creates the one chart and keeps it", async () => {
    const { engine, charts, loads, creates } = fakeEngine();
    render(<StrictMode>{inEnv(engine, <DashboardComponentView spec={SPECS.bar} data={fixtureData()} />)}</StrictMode>);
    const chart = await chartAt(charts);
    // Both mounts loaded the library; only the one still mounted created a chart.
    expect(loads()).toBe(2);
    expect(creates()).toBe(1);
    expect(charts).toHaveLength(1);
    expect(chart.disposed).toBe(false);
    act(() => chart.showTip(0, 1));
    expect(screen.getByTestId("tip-time").textContent).toBe("Tue");
  });
});

// Series values, categories and column names such as `hasOwnProperty` or `__proto__`. ECharts
// keyed its legend state by series name and threw on `hasOwnProperty`; its text measuring keyed by
// a drawn line and wrote onto Object.prototype for `__proto__` (chart/options.test.ts runs ECharts).
describe("names from the data", () => {
  const slots = PROTOTYPE_NAMES.map((_, slot) => seriesName(slot));

  test("series named like Object.prototype members are slots in the option and their names in the legend and tooltip", async () => {
    const { charts } = show(PROTOTYPE_SPECS.timeSeries, prototypeNameData());
    const chart = await chartAt(charts);
    expect(seriesNames(chart)).toEqual([...slots, THRESHOLD_SERIES]);
    expect(Object.keys((chart.option.legend as { selected: Record<string, boolean> }).selected)).toEqual(slots);
    for (const name of PROTOTYPE_NAMES) {
      expect(screen.getByRole("button", { name }).getAttribute("aria-pressed")).toBe("true");
    }
    fireEvent.click(screen.getByRole("button", { name: "hasOwnProperty" }));
    expect(chart.actions).toContainEqual({ type: "legendUnSelect", name: seriesName(0) });
    act(() => chart.showTip(1, 0));
    const tip = screen.getByTestId("chart-tooltip");
    expect(within(tip).queryByText("hasOwnProperty")).toBeNull();
    for (const name of PROTOTYPE_NAMES.slice(1)) expect(within(tip).getByText(name)).toBeTruthy();
    expect(screen.queryByTestId("crashed")).toBeNull();
  });

  test("categories named like them are indexes in the option, drawn through the formatter", async () => {
    const { charts } = show(PROTOTYPE_SPECS.bar, prototypeNameData());
    const chart = await chartAt(charts);
    expect(seriesNames(chart)).toEqual(slots.slice(0, 2));
    const xAxis = chart.option.xAxis as { data: string[]; axisLabel: { formatter(value: string): string } };
    expect(xAxis.data).toEqual(["0", "1", "2", "3"]);
    expect(xAxis.data.map((value) => xAxis.axisLabel.formatter(value))).toEqual(
      PROTOTYPE_NAMES.map((name) => chartText(name)),
    );
    act(() => chart.showTip(0, 1));
    expect(screen.getByTestId("tip-time").textContent).toBe("__proto__");
    fireEvent.click(screen.getByRole("button", { name: "__proto__" }));
    expect(chart.actions).toContainEqual({ type: "legendUnSelect", name: seriesName(1) });
  });

  test("y columns and a trend's column named like them", async () => {
    const wide = show(PROTOTYPE_SPECS.wide, prototypeNameData());
    expect(seriesNames(await chartAt(wide.charts))).toEqual(slots);
    for (const name of PROTOTYPE_NAMES) expect(screen.getByRole("button", { name })).toBeTruthy();
    wide.unmount();
    const kpi = show(PROTOTYPE_SPECS.kpi, prototypeNameData());
    const chart = await chartAt(kpi.charts);
    expect(seriesNames(chart)).toEqual([seriesName(0)]);
    act(() => chart.showTip(0, 0));
    // The first row of the `constructor` column.
    expect(tooltipValues()).toEqual([format.withUnit(format.number(2.125), "kWh")]);
  });

  test("a dataset named like them that is still loading shows the placeholder", () => {
    for (const name of PROTOTYPE_NAMES) {
      const data = fixtureData();
      const readings = data.datasets.find((dataset) => dataset.name === "readings");
      if (readings === undefined) throw new Error("fixture");
      // No result under the name yet: the lookup must not find Object.prototype's member.
      show({ ...SPECS.table, dataset: name }, { ...data, datasets: [...data.datasets, { ...readings, name }] });
      expect(screen.getByRole("status").textContent).toContain("Loading data…");
      expect(screen.queryByRole("alert")).toBeNull();
      cleanup();
    }
  });

  test("a column type named like them is a mismatch, not a lookup in Object.prototype", () => {
    // A type this client does not know, as a newer API could send.
    show(SPECS.timeSeries, withColumnType(fixtureData(), "power", "kw", "constructor" as never));
    expectProblem(/needs a number column/);
  });
});

describe("a component that fails", () => {
  test("ECharts throwing on an option turns only that card into an error, under its title", async () => {
    await withQuietErrors(async () => {
      const { engine, charts } = fakeEngine({
        setOptionError: new TypeError("source.hasOwnProperty is not a function"),
      });
      render(
        inEnv(
          engine,
          <>
            <DashboardComponentView spec={SPECS.bar} data={fixtureData()} />
            <DashboardComponentView spec={SPECS.table} data={fixtureData()} />
          </>,
        ),
      );
      const crashed = await screen.findByTestId("crashed");
      expect(crashed.textContent).toBe("Something went wrong while drawing it. The rest of the page is not affected.");
      const card = crashed.closest("section") as HTMLElement;
      expect(within(card).getByRole("heading", { name: "Energy by day" })).toBeTruthy();
      expect(within(card).getByRole("alert").textContent).toContain("This component can't be shown");
      expect(screen.getAllByTestId("crashed")).toHaveLength(1);
      // The table beside it is drawn as usual, and the failed chart was disposed with its card.
      expect(screen.getAllByTestId("table-cell")).toHaveLength(READINGS_ROWS.length * 5);
      expect(charts[0]?.disposed).toBe(true);
    });
  });

  test("a component that throws while rendering shows its card as an error; new data tries again", async () => {
    await withQuietErrors(() => {
      const data = fixtureData();
      const state = data.results.readings;
      if (state?.status !== "ready") throw new Error("fixture");
      // A result without its rows array, which the API never sends: the table fails to render.
      const broken: DashboardData = {
        ...data,
        results: { ...data.results, readings: { status: "ready", result: { ...state.result, rows: null as never } } },
      };
      const { engine } = fakeEngine();
      const page = (tableData: DashboardData) =>
        inEnv(
          engine,
          <>
            <DashboardComponentView spec={SPECS.table} data={tableData} />
            <DashboardComponentView spec={SPECS.status} data={data} />
          </>,
        );
      const view = render(page(broken));
      const crashed = screen.getByTestId("crashed");
      expect(within(crashed.closest("section") as HTMLElement).getByRole("heading", { name: "Meters" })).toBeTruthy();
      expect(screen.getByTestId("status-band").textContent).toBe(`Below Warning ${format.withUnit("100", "kW")}`);
      view.rerender(page(fixtureData()));
      expect(screen.queryByTestId("crashed")).toBeNull();
      expect(screen.getAllByTestId("table-cell")).toHaveLength(READINGS_ROWS.length * 5);
    });
  });
});
