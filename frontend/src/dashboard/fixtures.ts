// Test data for the dashboard components: one dataset per kind of chart, values with decimals
// that rounding would change, a missing value, and a computation and constants. Used by the
// component tests; a dataset or column here can be altered per test to make a mismatch.
import type { DashboardData, Dataset, DatasetResult } from "./data.ts";
import type { PlatformComponent } from "./spec.ts";

const QUERIED_AT = "2026-11-01T02:30:00Z";

/** A result of `dataset` with these columns and rows, as the API returns it. */
export function result(dataset: string, columns: DatasetResult["columns"], rows: DatasetResult["rows"]): DatasetResult {
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

/** A dataset the fixture data can declare: SQL, up to the contract's 2000 rows. */
export function sqlDataset(name: string, title: string, sql: string): Dataset {
  return { name, title, description: `${title}（測試資料）`, max_rows: 2000, source: { kind: "sql", sql } };
}

/** A dataset of the telemetry statistics or usage tool, whose source is the request the model wrote. */
export function requestDataset(
  kind: "telemetry_statistics" | "telemetry_usage",
  name: string,
  title: string,
  request: Extract<Dataset["source"], { kind: "telemetry_statistics" }>["request"],
): Dataset {
  return { name, title, description: `${title}（測試資料）`, max_rows: 2000, source: { kind, request } };
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
  bar: { kind: "bar", title: "Energy by day", dataset: "daily", x: "day", y: ["kwh"], series: "site", unit: "kWh" },
  kpi: {
    kind: "kpi",
    title: "Energy this week",
    value: { kind: "computation", name: "total_kwh" },
    compare: "last_week",
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
    value: { kind: "cell", dataset: "readings", column: "value", row: 0 },
    thresholds: ["warn", "limit"],
    unit: "kW",
  },
} satisfies Record<string, PlatformComponent>;

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
  wide: { kind: "time_series", title: "Wide power", dataset: "wide", x: "ts", y: [...PROTOTYPE_NAMES] },
  bar: SPECS.bar,
  kpi: { ...SPECS.kpi, trend: { dataset: "wide", x: "ts", y: "constructor" } },
} satisfies Record<string, PlatformComponent>;

const HOUR_MS = 3_600_000;

/**
 * A cold room's temperature and humidity, hourly for seven days (2026-10-31 to 2026-11-06) from a
 * gateway that is switched off every night after the second and the fourth day: no row from 18:00
 * to 10:00 (a hole of 17 hours between two rows, where a query that groups by time leaves the
 * buckets out). Day 1 also misses two rows (13:00 and 14:00): a hole of exactly three times the
 * spacing, which is not a gap and stays one line. The values follow the time of day.
 */
function climateRows(): DatasetResult["rows"] {
  const start = Date.UTC(2026, 9, 31);
  const rows: DatasetResult["rows"] = [];
  for (let hour = 0; hour < 7 * 24; hour++) {
    const day = Math.floor(hour / 24);
    const clock = hour % 24;
    const off = ((day === 2 || day === 4) && clock >= 18) || ((day === 3 || day === 5) && clock < 10);
    const short = day === 1 && (clock === 13 || clock === 14);
    if (off || short) continue;
    const wave = Math.sin(((clock - 9) / 24) * 2 * Math.PI);
    const drift = ((day * 7 + clock * 3) % 5) / 10;
    const time = new Date(start + hour * HOUR_MS).toISOString().slice(0, 19).replace("T", " ");
    rows.push([
      time,
      Math.round((4 + 1.8 * wave + drift) * 10) / 10,
      Math.round((62 - 9 * wave - drift * 2) * 10) / 10,
    ]);
  }
  return rows;
}

/** The datasets the gap fixture's components read (`GAP_SPECS`); not part of `fixtureData`. */
export const GAP_DATASETS: Dataset[] = [
  sqlDataset("climate", "Cold room climate", "SELECT ts, temperature, humidity FROM cold_room_hourly ORDER BY ts"),
];

/** The result of each of `GAP_DATASETS`, by name. */
export function gapResults(): Map<string, DatasetResult> {
  const rows = climateRows();
  return new Map([
    [
      "climate",
      result(
        "climate",
        [
          { name: "ts", type: "time" },
          { name: "temperature", type: "number" },
          { name: "humidity", type: "number" },
        ],
        rows,
      ),
    ],
  ]);
}

/**
 * A time series of two lines over the climate rows, and a KPI whose trend reads them: each line
 * and the trend break at the two nights without rows, and not at the short hole.
 */
export const GAP_SPECS = {
  timeSeries: {
    kind: "time_series",
    title: "Cold room temperature and humidity",
    dataset: "climate",
    x: "ts",
    y: ["temperature", "humidity"],
  },
  kpi: {
    kind: "kpi",
    title: "Cold room temperature now",
    value: { kind: "cell", dataset: "climate", column: "temperature", row: climateRows().length - 1 },
    trend: { dataset: "climate", x: "ts", y: "temperature" },
    unit: "°C",
  },
} satisfies Record<string, PlatformComponent>;
