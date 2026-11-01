// The options the charts hand ECharts, checked against ECharts itself where a fake engine cannot
// tell: ECharts 6 runs here with its SVG renderer in server-side mode (no canvas needed), on the
// same chart and component modules the app registers.
import { describe, expect, test } from "bun:test";
import { init, use } from "echarts/core";
import { SVGRenderer } from "echarts/renderers";

import type { DashboardData } from "../data.ts";
import { fixtureData, PROTOTYPE_NAMES, PROTOTYPE_SPECS, prototypeNameData, SPECS } from "../fixtures.ts";
import { dashboardFormat } from "../format.ts";
import { componentModel } from "../model.ts";
import type { ComponentSpec } from "../spec.ts";
import { CHART_MODULES } from "./echarts.ts";
import type { ChartOption, ChartTheme } from "./engine.ts";
import type { ChartEnvironment } from "./InteractiveChart.tsx";
import {
  barOption,
  chartText,
  seriesName,
  sparkOption,
  THRESHOLD_SERIES,
  timeSeriesOption,
  withSelection,
} from "./options.ts";

use([...CHART_MODULES, SVGRenderer]);

const format = dashboardFormat("en", "UTC");

/** The light theme's colors, as `readTheme` hands them over. */
const THEME: ChartTheme = {
  series: [
    "rgba(11, 132, 102, 1.000)",
    "rgba(42, 120, 214, 1.000)",
    "rgba(212, 87, 42, 1.000)",
    "rgba(98, 80, 214, 1.000)",
    "rgba(194, 66, 122, 1.000)",
    "rgba(138, 109, 0, 1.000)",
  ],
  text: "rgba(23, 23, 23, 1.000)",
  muted: "rgba(98, 98, 98, 1.000)",
  grid: "rgba(229, 229, 229, 1.000)",
  axis: "rgba(163, 163, 163, 1.000)",
  surface: "rgba(255, 255, 255, 1.000)",
  font: "sans-serif",
};

const ENVIRONMENTS: ChartEnvironment[] = [
  { theme: THEME, animate: true, finePointer: true },
  { theme: THEME, animate: false, finePointer: false },
];

interface ChartCase {
  name: string;
  option: ChartOption;
  /** The slots of the option's data series. */
  slots: number[];
  /** Labels the chart draws along its x axis, when they come from the data. */
  categories: string[];
}

/** Every chart a spec draws over `data`, with the option it hands ECharts. */
function chartsOf(spec: ComponentSpec, data: DashboardData, environment: ChartEnvironment): ChartCase[] {
  const model = componentModel(spec, data);
  if (model.state !== "ready") throw new Error(`${spec.title} is ${model.state}`);
  const { view } = model;
  switch (view.kind) {
    case "time_series":
      return [
        {
          name: spec.title,
          option: timeSeriesOption(view, format, environment, spec.title),
          slots: view.series.map((series) => series.slot),
          categories: [],
        },
      ];
    case "bar":
      return [
        {
          name: spec.title,
          option: barOption(view, format, environment, spec.title),
          slots: view.series.map((series) => series.slot),
          categories: view.categories.map((category) => category.label),
        },
      ];
    case "kpi":
      return view.trend === null
        ? []
        : [
            {
              name: `${spec.title} trend`,
              option: sparkOption(view.trend.series, environment, spec.title),
              slots: [0],
              categories: [],
            },
          ];
    default:
      return [];
  }
}

/** The fixture's charts, then the same with `PROTOTYPE_NAMES` as series, categories and columns. */
function allCharts(): ChartCase[] {
  const cases: Array<[ComponentSpec, DashboardData]> = [
    [SPECS.timeSeries, fixtureData()],
    [SPECS.bar, fixtureData()],
    [SPECS.kpi, fixtureData()],
    ...Object.values(PROTOTYPE_SPECS).map((spec): [ComponentSpec, DashboardData] => [spec, prototypeNameData()]),
  ];
  return ENVIRONMENTS.flatMap((environment) => cases.flatMap(([spec, data]) => chartsOf(spec, data, environment)));
}

/**
 * The own properties of `Object.prototype`, of `Object` and of each function on the prototype:
 * where a library that looks a name up in a plain object writes when the name is one of them.
 */
function prototypeState(): Record<string, string[]> {
  const state: Record<string, string[]> = {
    prototype: Object.getOwnPropertyNames(Object.prototype).sort(),
    Object: Object.getOwnPropertyNames(Object).sort(),
  };
  for (const name of Object.getOwnPropertyNames(Object.prototype)) {
    const value = Object.getOwnPropertyDescriptor(Object.prototype, name)?.value;
    if (typeof value === "function") state[`prototype.${name}`] = Object.getOwnPropertyNames(value).sort();
  }
  return state;
}

/** Every object under a `tooltip` key, at any depth of `value`, with where it is. */
function tooltipsIn(value: unknown, path: string): Array<{ path: string; tooltip: Record<string, unknown> }> {
  if (Array.isArray(value)) return value.flatMap((item, index) => tooltipsIn(item, `${path}[${index}]`));
  if (value === null || typeof value !== "object") return [];
  return Object.entries(value).flatMap(([key, child]) => [
    ...(key === "tooltip" && child !== null && typeof child === "object"
      ? [{ path: `${path}.${key}`, tooltip: child as Record<string, unknown> }]
      : []),
    ...tooltipsIn(child, `${path}.${key}`),
  ]);
}

describe("names", () => {
  test("no name ECharts keys its state by comes from the data", () => {
    const cases = allCharts();
    // Two environments, each with the fixture's three charts and the four prototype-name ones.
    expect(cases.length).toBe(14);
    for (const chart of cases) {
      const option = withSelection(chart.option, chart.slots, new Set());
      const names = (option.series as Array<{ name: string }>).map((series) => series.name);
      for (const name of names) expect([...chart.slots.map(seriesName), THRESHOLD_SERIES]).toContain(name);
      const selected = (option.legend as { selected: Record<string, boolean> }).selected;
      expect(Object.keys(selected)).toEqual(chart.slots.map(seriesName));
      // Functions (the label formatters) are left out: what they return is checked below.
      const text = JSON.stringify(option);
      for (const name of PROTOTYPE_NAMES) expect(text).not.toContain(name);
    }
  });

  test("a label line named like an Object.prototype member is drawn as a key of its own", () => {
    for (const name of [...PROTOTYPE_NAMES, "valueOf", "isPrototypeOf", "__defineGetter__"]) {
      const text = chartText(name);
      expect(text in Object.prototype).toBe(false);
      expect(text).toBe(`\u2060${name}`);
    }
    const lines = chartText("Limit\n__proto__\nconstructor").split("\n");
    expect(lines.map((line) => line in Object.prototype)).toEqual([false, false, false]);
    for (const plain of ["Mon", "Limit 1,245.5 kW", "1.2K", ""]) expect(chartText(plain)).toBe(plain);
  });
});

describe("ECharts", () => {
  test("takes every option and legend action and leaves Object.prototype alone", () => {
    const before = prototypeState();
    for (const chart of allCharts()) {
      // Measuring a drawn line `__proto__` wrote `prev` and `next` onto Object.prototype, and the
      // SVG renderer's own `for in` loops then failed on them.
      const unchanged = (step: string) =>
        expect({ chart: chart.name, step, state: prototypeState() }).toEqual({
          chart: chart.name,
          step,
          state: before,
        });
      const instance = init(null, null, { renderer: "svg", ssr: true, width: 640, height: 320 });
      try {
        const first = chart.slots[0] as number;
        instance.setOption(withSelection(chart.option, chart.slots, new Set([first])), { notMerge: true });
        unchanged("setOption");
        for (const slot of chart.slots) {
          instance.dispatchAction({ type: "legendSelect", name: seriesName(slot) });
          instance.dispatchAction({ type: "legendUnSelect", name: seriesName(slot) });
          instance.dispatchAction({ type: "legendSelect", name: seriesName(slot) });
        }
        instance.dispatchAction({ type: "showTip", seriesIndex: 0, dataIndex: 0 });
        instance.dispatchAction({ type: "hideTip" });
        unchanged("actions");
        const svg = instance.renderToSVGString();
        // The categories' labels are drawn, as the axis formatter returns them.
        for (const label of chart.categories) expect(svg).toContain(`>${chartText(label)}<`);
      } finally {
        instance.dispose();
      }
      unchanged("dispose");
    }
  });
});

describe("tooltip", () => {
  test("ECharts never builds tooltip content: every tooltip in every option hides it and has no formatter", () => {
    for (const chart of allCharts()) {
      const option = withSelection(chart.option, chart.slots, new Set());
      // ECharts' HTML tooltip writes innerHTML; the global setting keeps it from showing anything.
      expect((option.tooltip as { showContent?: unknown }).showContent).toBe(false);
      const tooltips = tooltipsIn(option, "option");
      expect(tooltips.length).toBeGreaterThan(0);
      for (const { path, tooltip } of tooltips) {
        expect({ path, showContent: tooltip.showContent === true }).toEqual({ path, showContent: false });
        expect({ path, formatter: "formatter" in tooltip }).toEqual({ path, formatter: false });
      }
    }
  });
});
