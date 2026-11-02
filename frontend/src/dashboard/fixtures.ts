// Test data for the dashboard components: one dataset per kind of chart, values with decimals
// that rounding would change, a missing value, and a computation and constants. Used by the
// component tests; a dataset or column here can be altered per test to make a mismatch.
import type { DashboardData, Dataset, DatasetResult } from "./data.ts";
import type { ComponentSpec } from "./spec.ts";

const QUERIED_AT = "2026-11-01T02:30:00Z";

function result(dataset: string, columns: DatasetResult["columns"], rows: DatasetResult["rows"]): DatasetResult {
  return {
    dataset,
    columns,
    rows,
    row_count: rows.length,
    meta: {
      queried_at: QUERIED_AT,
      duration_ms: 12,
      source: {
        kind: "sql",
        queryable_from: {
          tenant_telemetry: "2026-10-25T00:00:00Z",
          tenant_telemetry_1h: "2025-11-01T00:00:00Z",
          tenant_telemetry_1m: "2026-10-02T00:00:00Z",
        },
      },
    },
  };
}

function sqlDataset(name: string, title: string, sql: string): Dataset {
  return { name, title, description: `${title}（測試資料）`, max_rows: 2000, source: { kind: "sql", sql } };
}

export const POWER_ROWS: DatasetResult["rows"] = [
  ["2026-11-01 00:00:00", "Line A", 12.3456],
  ["2026-11-01 00:00:00", "Line B", 1234.5],
  ["2026-11-01 00:15:00", "Line A", 0.000123],
  ["2026-11-01 00:15:00", "Line B", null],
  ["2026-11-01 00:30:00", "Line A", -7.25],
  ["2026-11-01 00:30:00", "Line B", 1240.125],
  ["2026-11-01 00:45:00", "Line B", 1250],
];

export const DAILY_ROWS: DatasetResult["rows"] = [
  ["Mon", "Site 1", 120.5],
  ["Mon", "Site 2", 98.25],
  ["Tue", "Site 1", 133.333],
  ["Tue", "Site 2", 101],
  ["Wed", "Site 1", 0.5],
];

export const READINGS_ROWS: DatasetResult["rows"] = [
  ["Meter 1", 42.1234, true, "2026-10-31", "2026-11-01 01:02:03"],
  ["Meter 2", null, false, "2026-10-30", "2026-11-01 01:05:00"],
  ["Meter 3", 1000000.5, null, null, "2026-11-01 01:06:07.250"],
];

export const TREND_ROWS: DatasetResult["rows"] = [
  ["2026-10-26", 310.25],
  ["2026-10-27", 298],
  ["2026-10-28", 305.875],
  ["2026-10-29", null],
  ["2026-10-30", 321.5],
];

export function fixtureData(): DashboardData {
  return {
    datasets: [
      sqlDataset("power", "Line power", "SELECT ts, line, kw FROM power ORDER BY ts"),
      sqlDataset("daily", "Daily energy", "SELECT day, site, kwh FROM daily"),
      sqlDataset("readings", "Meter readings", "SELECT meter, value, online, installed, seen FROM readings"),
      sqlDataset("trend", "Daily total", "SELECT day, kwh FROM daily_total ORDER BY day"),
    ],
    computations: [
      { name: "total_kwh", title: "Total energy", formula: "sum(daily.kwh)", unit: "kWh" },
      { name: "last_week", title: "Last week", formula: "daily.kwh[1] * 2", unit: "kWh" },
    ],
    constants: [
      { name: "warn", title: "Warning", value: 100, unit: "kW" },
      { name: "limit", title: "Limit", value: 1245.5, unit: "kW", note: "Contract demand" },
    ],
    results: {
      power: {
        status: "ready",
        result: result(
          "power",
          [
            { name: "ts", type: "time" },
            { name: "line", type: "text" },
            { name: "kw", type: "number" },
          ],
          POWER_ROWS,
        ),
      },
      daily: {
        status: "ready",
        result: result(
          "daily",
          [
            { name: "day", type: "text" },
            { name: "site", type: "text" },
            { name: "kwh", type: "number" },
          ],
          DAILY_ROWS,
        ),
      },
      readings: {
        status: "ready",
        result: result(
          "readings",
          [
            { name: "meter", type: "text" },
            { name: "value", type: "number" },
            { name: "online", type: "bool" },
            { name: "installed", type: "time" },
            { name: "seen", type: "time" },
          ],
          READINGS_ROWS,
        ),
      },
      trend: {
        status: "ready",
        result: result(
          "trend",
          [
            { name: "day", type: "time" },
            { name: "kwh", type: "number" },
          ],
          TREND_ROWS,
        ),
      },
    },
  };
}

export const SPECS = {
  timeSeries: {
    kind: "time_series",
    title: "Power by line",
    dataset: "power",
    x: "ts",
    y: ["kw"],
    series: "line",
    unit: "kW",
    thresholds: ["limit"],
  },
  bar: { kind: "bar", title: "Energy by day", dataset: "daily", x: "day", y: "kwh", series: "site", unit: "kWh" },
  kpi: {
    kind: "kpi",
    title: "Energy this week",
    value: { computation: "total_kwh" },
    comparison: "last_week",
    trend: { dataset: "trend", x: "day", y: "kwh" },
    unit: "kWh",
  },
  table: {
    kind: "table",
    title: "Meters",
    dataset: "readings",
    columns: [
      { column: "meter" },
      { column: "value", unit: "kWh" },
      { column: "online" },
      { column: "installed" },
      { column: "seen" },
    ],
  },
  status: {
    kind: "status",
    title: "Meter 1",
    value: { dataset: "readings", column: "value", row: 0 },
    thresholds: ["warn", "limit"],
    unit: "kW",
  },
} satisfies Record<string, ComponentSpec>;

/**
 * Names of `Object.prototype` members. A series value, a category or a column can be any of
 * them, and a library that keeps state in plain objects keyed by name breaks on them.
 */
export const PROTOTYPE_NAMES = ["hasOwnProperty", "__proto__", "constructor", "toString"] as const;

const PROTOTYPE_TIMES = ["2026-11-01 00:00:00", "2026-11-01 00:15:00", "2026-11-01 00:30:00"];

/**
 * The fixture data with `PROTOTYPE_NAMES` everywhere a value becomes a name: the power lines, the
 * daily categories and sites, and a `wide` dataset whose number columns are named after them.
 */
export function prototypeNameData(): DashboardData {
  const data = fixtureData();
  const names = [...PROTOTYPE_NAMES];
  const power: DatasetResult["rows"] = names.flatMap((name, index) =>
    PROTOTYPE_TIMES.map((time, step): DatasetResult["rows"][number] => [time, name, index * 10 + step + 0.5]),
  );
  const daily: DatasetResult["rows"] = names.flatMap((day, index) =>
    names.slice(0, 2).map((site, offset): DatasetResult["rows"][number] => [day, site, index * 2 + offset + 0.25]),
  );
  const wide: DatasetResult["rows"] = PROTOTYPE_TIMES.map((time, step) => [
    time,
    ...names.map((_, index) => index + step + 0.125),
  ]);
  const ready = (state: DashboardData["results"][string] | undefined, rows: DatasetResult["rows"]) => {
    if (state?.status !== "ready") throw new Error("the fixture has every result");
    return { status: "ready" as const, result: { ...state.result, rows, row_count: rows.length } };
  };
  return {
    ...data,
    datasets: [...data.datasets, sqlDataset("wide", "Wide power", "SELECT ts, ... FROM wide ORDER BY ts")],
    results: {
      ...data.results,
      power: ready(data.results.power, power),
      daily: ready(data.results.daily, daily),
      wide: {
        status: "ready",
        result: result(
          "wide",
          [{ name: "ts", type: "time" }, ...names.map((name) => ({ name, type: "number" as const }))],
          wide,
        ),
      },
    },
  };
}

/** Specs that turn `PROTOTYPE_NAMES` into series, categories and a trend's column. */
export const PROTOTYPE_SPECS = {
  timeSeries: SPECS.timeSeries,
  wide: { kind: "time_series", title: "Wide power", dataset: "wide", x: "ts", y: [...PROTOTYPE_NAMES], mark: "area" },
  bar: SPECS.bar,
  kpi: { ...SPECS.kpi, trend: { dataset: "wide", x: "ts", y: "constructor" } },
} satisfies Record<string, ComponentSpec>;
