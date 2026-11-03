// The dashboard contract's own samples (crates/contract/tests/fixtures/dashboard/, written from the
// Rust types and checked against them by `cargo test -p contract`) through the components: a
// version's `spec.components` as `GET /api/assistant/v1/dashboards/{id}` sends it, with the results
// of the datasets it names, reaches `DashboardComponent` without a conversion in between. Every
// number on the screen is a value of those results, formatted.
import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";

import type { components } from "../api/schema.gen.ts";
import { type FakeChart, fakeEngine } from "../test/fake-chart.ts";
import { ChartEngineContext } from "./chart/engine.ts";
import { DashboardEnvProvider } from "./context.tsx";
import { DashboardComponentView } from "./DashboardComponent.tsx";
import type { DashboardData, DatasetResult } from "./data.ts";
import { dashboardFormat, wallMs } from "./format.ts";
import { type ComponentSpec, isPlatformComponent, parseComponentSpec } from "./spec.ts";

afterEach(() => {
  cleanup();
});

const DIRECTORY = new URL("../../../crates/contract/tests/fixtures/dashboard/", import.meta.url);

function fixture<T>(name: string): T {
  return JSON.parse(readFileSync(new URL(name, DIRECTORY), "utf8")) as T;
}

type Spec = components["schemas"]["DashboardSpec"];
type View = components["schemas"]["DashboardView"];

const SPEC = fixture<Spec>("spec-components.json");
const VIEW = fixture<View>("dashboard-view-components.json");
const RESULTS = [
  "result-sql.json",
  "result-statistics-total.json",
  "result-statistics-buckets.json",
  "result-usage-buckets.json",
  "result-usage-total.json",
].map((name) => fixture<DatasetResult>(name));

const format = dashboardFormat("en", "UTC");

/** The spec's datasets, computations and constants, and the results the contract's samples have. */
function sampleData(spec: Spec): DashboardData {
  return {
    datasets: spec.datasets,
    computations: spec.computations ?? [],
    constants: spec.constants ?? [],
    results: Object.fromEntries(
      spec.datasets.map((dataset) => {
        const result = RESULTS.find((candidate) => candidate.dataset === dataset.name);
        if (result === undefined) throw new Error(`no sample result for ${dataset.name}`);
        return [dataset.name, { status: "ready" as const, result }];
      }),
    ),
  };
}

function resultOf(name: string): DatasetResult {
  const result = RESULTS.find((candidate) => candidate.dataset === name);
  if (result === undefined) throw new Error(`no sample result for ${name}`);
  return result;
}

function column(result: DatasetResult, name: string): number {
  const index = result.columns.findIndex((candidate) => candidate.name === name);
  if (index < 0) throw new Error(`no column ${name} in ${result.dataset}`);
  return index;
}

/** The numbers of one column. */
function numbers(result: DatasetResult, name: string): number[] {
  const index = column(result, name);
  return result.rows.map((row) => row[index] as number);
}

function platformComponents(spec: Spec): ComponentSpec[] {
  return (spec.components ?? []).filter(isPlatformComponent);
}

function component(spec: Spec, kind: string): Extract<ComponentSpec, { kind: string }> {
  const found = (spec.components ?? []).find((item) => item.kind === kind);
  if (found === undefined) throw new Error(`the sample has no ${kind}`);
  return found;
}

function show(spec: ComponentSpec, data: DashboardData) {
  if (!isPlatformComponent(spec)) throw new Error("the custom page is not drawn by a component");
  const { engine, charts } = fakeEngine();
  render(
    <ChartEngineContext.Provider value={engine}>
      <DashboardEnvProvider locale="en" timeZone="UTC">
        <DashboardComponentView spec={spec} data={data} />
      </DashboardEnvProvider>
    </ChartEngineContext.Provider>,
  );
  return charts;
}

async function chartOf(charts: FakeChart[]): Promise<FakeChart> {
  await waitFor(() => {
    if (charts[0] === undefined || charts[0].options.length === 0) throw new Error("waiting for the chart's option");
  });
  return charts[0] as FakeChart;
}

describe("the contract's component samples", () => {
  test("every component of the sample reads as the schema, and the custom page is left to the page", () => {
    const list = SPEC.components ?? [];
    expect(list.map((item) => item.kind)).toEqual(["kpi", "status", "time_series", "bar", "table", "custom_page"]);
    // The same objects: parsing changes nothing the API sent.
    for (const item of list) expect(parseComponentSpec(item)).toEqual(item);
    expect(platformComponents(SPEC).map((item) => item.kind)).toEqual(["kpi", "status", "time_series", "bar", "table"]);
    // A version of components only has no custom page item and no page.
    const view = VIEW.version.spec.components ?? [];
    expect(view.every(isPlatformComponent)).toBe(true);
    for (const item of view) expect(parseComponentSpec(item)).toEqual(item);
  });

  test("every component of the sample is drawn, none with a problem", async () => {
    const data = sampleData(SPEC);
    for (const spec of platformComponents(SPEC)) {
      show(spec, data);
      expect(screen.queryByRole("alert")).toBeNull();
      cleanup();
    }
    const view = sampleData(VIEW.version.spec);
    for (const spec of VIEW.version.spec.components ?? []) {
      show(spec, view);
      expect(screen.queryByRole("alert")).toBeNull();
      cleanup();
    }
  });

  test("the KPI shows its computation's value and its comparison with the computation's own unit", () => {
    show(component(SPEC, "kpi"), sampleData(SPEC));
    const usage = numbers(resultOf("meter_usage"), "usage").reduce((sum, value) => sum + value, 0);
    expect(screen.getByTestId("value").textContent).toBe(format.number(usage));
    // `carbon_kg` is sum(meter_usage.usage) * 0.494 in kgCO2e; the KPI's own unit is kWh.
    expect(screen.getByTestId("comparison").textContent).toBe(format.withUnit(format.number(usage * 0.494), "kgCO2e"));
  });

  test("the KPI's trend is the rows of its dataset", async () => {
    const charts = show(component(SPEC, "kpi"), sampleData(SPEC));
    const chart = await chartOf(charts);
    const result = resultOf("meter_usage");
    const times = result.rows.map((row) => wallMs(row[column(result, "time")] ?? null) as number);
    const points = (chart.option.series as Array<{ data: Array<[number, number | null]> }>)[0]?.data;
    expect(points).toEqual(numbers(result, "usage").map((usage, index) => [times[index] as number, usage]));
  });

  test("the status shows the cell of row 0 and the band between its two thresholds", () => {
    show(component(SPEC, "status"), sampleData(SPEC));
    const result = resultOf("meter_stats");
    const average = result.rows[0]?.[column(result, "avg")] as number;
    expect(screen.getByTestId("value").textContent).toBe(format.number(average));
    // power_warn is 30 kW and power_alarm 40 kW: the sample's average is below the first.
    expect(average).toBeLessThan(30);
    expect(screen.getByTestId("status-band").textContent).toBe(`Below Power warning ${format.withUnit("30", "kW")}`);
  });

  test("the time series draws one line per device, every point a row's value", async () => {
    const charts = show(component(SPEC, "time_series"), sampleData(SPEC));
    const chart = await chartOf(charts);
    const result = resultOf("meter_usage");
    const lines = (chart.option.series as Array<{ data: Array<[number, number | null]> }>).filter(
      (item) => item.data.length > 0,
    );
    expect(lines).toHaveLength(1);
    expect(lines[0]?.data.map((point) => point[1])).toEqual(numbers(result, "usage"));
  });

  test("the bar chart draws the sample's days against its threshold", async () => {
    const charts = show(component(SPEC, "bar"), sampleData(SPEC));
    const chart = await chartOf(charts);
    const series = chart.option.series as Array<{
      data: Array<number | null>;
      markLine?: { data: Array<{ yAxis: number; label: { formatter(): string } }> };
    }>;
    const result = resultOf("daily_energy");
    expect(series[0]?.data).toEqual(result.rows.map((row) => row[column(result, "kwh")] as number | null));
    // The threshold is a reference line on the helper series, labelled with the constant's value.
    const line = series.find((item) => item.markLine !== undefined)?.markLine?.data[0];
    expect(line?.yAxis).toBe(1200);
    expect(line?.label.formatter()).toBe(`Weekly target ${format.withUnit(format.number(1200), "kWh")}`);
  });

  test("the table's cells are its dataset's rows, formatted by their column's type", () => {
    const spec = component(SPEC, "table");
    show(spec, sampleData(SPEC));
    const result = resultOf("meter_stats");
    const names = ["device_id", "status", "avg", "max_time"];
    const expected = result.rows.flatMap((row) =>
      names.map((name) => {
        const index = column(result, name);
        return format.cell(result.columns[index]?.type ?? "text", row[index] ?? null);
      }),
    );
    expect(screen.getAllByTestId("table-cell").map((cell) => cell.textContent)).toEqual(expected);
    expect(screen.getByRole("columnheader", { name: "avg (kW)" })).toBeTruthy();
  });

  test("a result that no longer fits the sample is a problem in the contract's words", async () => {
    const data = sampleData(SPEC);
    const state = data.results.meter_stats;
    if (state?.status !== "ready") throw new Error("sample");
    const changed: DashboardData = {
      ...data,
      results: {
        ...data.results,
        meter_stats: {
          status: "ready",
          result: { ...state.result, columns: state.result.columns.filter((item) => item.name !== "avg") },
        },
      },
    };
    show(component(SPEC, "status"), changed);
    expect(screen.getByRole("alert").textContent).toContain("The data has no column this item uses.");
    await act(async () => {});
  });
});
