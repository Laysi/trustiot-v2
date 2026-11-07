// The one mapping from a component spec and the data it names to what the component draws
// (docs/architecture/ai-assistant.md, 「兩條路的正確性」). It checks at viewing time what the
// assistant checked at writing time, because a result can change between the two (a SQL column's
// type, a second row for the same point): every column the spec names exists and has the type
// the component needs, a series has at most one row per x, references resolve. Anything else is
// a `Problem`, and the component shows it with its source instead of guessing.
//
// The view keeps every value as the result holds it, with its row: a point, a bar or a cell is
// one row's value, never a sum, an average or a converted unit. Splitting rows into series only
// separates them. Sorting a line's points by time changes no value.
import { cellOf, checkFormula, type Expr, evaluateFormula, parseFormula } from "../../sandbox/src/formula.ts";
import {
  type Cell,
  type ColumnType,
  cellAt,
  columnOf,
  type DashboardData,
  type DatasetResult,
  datasetOf,
  stateOf,
} from "./data.ts";
import { isDateOnly, wallMs } from "./format.ts";
import {
  type BarComponent,
  type KpiComponent,
  MAX_SERIES,
  type PlatformComponent,
  type StatusComponent,
  type TableComponent,
  type TimeSeriesComponent,
  type ValueReference,
} from "./spec.ts";

/**
 * The column types a component field accepts, as the contract names them (`Expect` in
 * component.rs): a time series' x is `time`, a bar chart's `category`, a y or a value `number`, a
 * series `text`, and a table shows any (a type this build does not know, from a newer API, is
 * the one thing that fails).
 */
export type ColumnExpectation = "time" | "number" | "text" | "category" | "any";
const COLUMN_TYPES: Readonly<Record<ColumnExpectation, readonly ColumnType[]>> = {
  time: ["time"],
  number: ["number"],
  text: ["text"],
  category: ["text", "time"],
  any: ["number", "text", "time", "bool"],
};

/** Where a shown value comes from: the rows of a dataset, a computation or a constant. */
export type SourceRef =
  | { kind: "rows"; dataset: string; rows: readonly number[] }
  | { kind: "computation"; name: string }
  | { kind: "constant"; name: string };

/** Whether two references name the same source: the same rows of one dataset, computation or constant. */
export function sameSource(a: SourceRef, b: SourceRef): boolean {
  switch (a.kind) {
    case "rows":
      return (
        b.kind === "rows" &&
        a.dataset === b.dataset &&
        a.rows.length === b.rows.length &&
        a.rows.every((row, index) => row === b.rows[index])
      );
    case "computation":
      return b.kind === "computation" && a.name === b.name;
    case "constant":
      return b.kind === "constant" && a.name === b.name;
  }
}

/**
 * Why a component cannot draw its data. A closed set; each has a message (`problemMessage`). The
 * codes the contract also checks at write time (`SpecProblem`, spec.rs) are named as it names
 * them and say it in the same message (`dashboard.spec.*`); the rest are only known at viewing.
 */
export type Problem =
  | { code: "dataset_unknown"; dataset: string }
  | { code: "dataset_failed"; dataset: string; message: string }
  | { code: "column_unknown"; dataset: string; column: string }
  | { code: "column_type"; dataset: string; column: string; expected: ColumnExpectation }
  | { code: "x_duplicate"; dataset: string; column: string; rows: readonly [number, number] }
  | { code: "missing_x"; dataset: string; column: string; row: number }
  | { code: "series_too_many"; dataset: string; count: number }
  | { code: "row_out_of_range"; dataset: string; row: number }
  | { code: "row_missing"; dataset: string; row: number }
  | { code: "computation_unknown"; name: string }
  | { code: "computation_invalid"; name: string }
  | { code: "constant_unknown"; name: string }
  | { code: "threshold_not_number"; name: string }
  | { code: "thresholds_not_ascending"; names: readonly string[] };

export interface Point {
  /** Milliseconds of the wall-clock time's UTC fields (format.ts). */
  x: number;
  y: number | null;
  /** The row in the dataset's result. */
  row: number;
}

export interface SeriesView {
  /** Stable within the component and unique: the series value and the y column together. */
  key: string;
  /** What the legend and the tooltip call it: the series value, the y column or both, as text. */
  label: string;
  /** 0-based color slot (`--chart-1` is 0); a series keeps its slot when others are hidden. */
  slot: number;
  /** The y column it draws. */
  column: string;
  points: readonly Point[];
}

export interface Threshold {
  name: string;
  title: string;
  value: number;
  unit: string | undefined;
}

export interface TimeSeriesView {
  kind: "time_series";
  title: string;
  unit: string | undefined;
  dataset: string;
  xColumn: string;
  /** Whether the x column holds dates only. */
  dateOnly: boolean;
  series: readonly SeriesView[];
  /** Every x of every series, ascending, without repeats: what the keyboard steps through. */
  xs: readonly number[];
  thresholds: readonly Threshold[];
}

export interface BarCategory {
  /** The x cell as written (text) or its wall-clock milliseconds (time). */
  label: string;
  time: number | null;
}

export interface BarSeriesView {
  key: string;
  label: string;
  slot: number;
  /** One per category: the row's value, or `null` where this series has no row. */
  values: readonly ({ y: number | null; row: number } | null)[];
}

export interface BarView {
  kind: "bar";
  title: string;
  unit: string | undefined;
  dataset: string;
  xColumn: string;
  xType: "text" | "time";
  dateOnly: boolean;
  categories: readonly BarCategory[];
  series: readonly BarSeriesView[];
  thresholds: readonly Threshold[];
}

/** One value a KPI or status shows, and where it comes from. */
export interface ResolvedValue {
  value: number | null;
  /** What the value is called in the source panel: the column, or the computation's title. */
  label: string;
  unit: string | undefined;
  source: SourceRef;
}

export interface KpiView {
  kind: "kpi";
  title: string;
  value: ResolvedValue;
  trend: { dataset: string; series: SeriesView; dateOnly: boolean } | null;
  comparison: (ResolvedValue & { title: string }) | null;
}

export interface TableColumnView {
  name: string;
  type: ColumnType;
  unit: string | undefined;
  index: number;
}

export interface TableView {
  kind: "table";
  title: string;
  dataset: string;
  columns: readonly TableColumnView[];
  result: DatasetResult;
}

export interface StatusView {
  kind: "status";
  title: string;
  value: ResolvedValue;
  thresholds: readonly Threshold[];
  /** Which band the value is in: 0 below the first threshold, n at or above the last; `null` without a value. */
  band: number | null;
}

export type ComponentView = TimeSeriesView | BarView | KpiView | TableView | StatusView;

export type ComponentModel =
  | { state: "loading" }
  | { state: "error"; problem: Problem; sources: readonly SourceRef[] }
  | { state: "ready"; view: ComponentView };

/** Thrown inside the mapping and turned into a model; never escapes this module. */
class Stop {
  constructor(readonly model: Exclude<ComponentModel, { state: "ready" }>) {}
}

function fail(problem: Problem, sources: readonly SourceRef[] = []): never {
  throw new Stop({ state: "error", problem, sources });
}

function wholeDataset(dataset: string): SourceRef {
  return { kind: "rows", dataset, rows: [] };
}

/** The result of `dataset`, or a stop: loading, failed, or not in the spec. */
function readyResult(data: DashboardData, dataset: string): DatasetResult {
  if (datasetOf(data, dataset) === undefined) fail({ code: "dataset_unknown", dataset });
  const state = stateOf(data, dataset);
  if (state.status === "loading") throw new Stop({ state: "loading" });
  if (state.status === "failed") {
    fail({ code: "dataset_failed", dataset, message: state.message }, [wholeDataset(dataset)]);
  }
  return state.result;
}

function typedColumn(
  result: DatasetResult,
  column: string,
  expected: ColumnExpectation,
): { index: number; type: ColumnType } {
  const found = columnOf(result, column);
  const sources = [wholeDataset(result.dataset)];
  if (found === undefined) fail({ code: "column_unknown", dataset: result.dataset, column }, sources);
  if (!COLUMN_TYPES[expected].includes(found.type)) {
    fail({ code: "column_type", dataset: result.dataset, column, expected }, sources);
  }
  return found;
}

function numberCell(cell: Cell): number | null {
  return typeof cell === "number" && Number.isFinite(cell) ? cell : null;
}

/** The x of every row as wall-clock milliseconds; a row without a time is a problem. */
function timeOf(result: DatasetResult, column: string, index: number, row: number): number {
  const ms = wallMs(cellAt(result, row, index));
  if (ms === null) fail({ code: "missing_x", dataset: result.dataset, column, row }, [wholeDataset(result.dataset)]);
  return ms;
}

function thresholdsOf(data: DashboardData, names: readonly string[] | undefined): Threshold[] {
  return (names ?? []).map((name) => {
    const constant = data.constants.find((candidate) => candidate.name === name);
    if (constant === undefined) fail({ code: "constant_unknown", name });
    if (typeof constant.value !== "number") {
      fail({ code: "threshold_not_number", name }, [{ kind: "constant", name }]);
    }
    return { name, title: constant.title, value: constant.value, unit: constant.unit ?? undefined };
  });
}

/** Text of a series cell: the value as written, `—` for `null`. */
function seriesLabel(cell: Cell): string {
  return cell === null ? "—" : String(cell);
}

/**
 * Splits rows into the groups of the `series` column, in order of first appearance (one group of
 * every row without a column), and checks that every group gets a line or bars for each of the
 * `yCount` columns: no more than the colors there are (the contract's `series_too_many`).
 */
function splitSeries(result: DatasetResult, column: string | null | undefined, yCount: number): Map<string, number[]> {
  const groups = new Map<string, number[]>();
  if (column === undefined || column === null) {
    groups.set(
      "",
      result.rows.map((_, row) => row),
    );
  } else {
    const { index } = typedColumn(result, column, "text");
    for (let row = 0; row < result.rows.length; row++) {
      const key = seriesLabel(cellAt(result, row, index));
      const rows = groups.get(key);
      if (rows === undefined) groups.set(key, [row]);
      else rows.push(row);
    }
  }
  const count = groups.size * yCount;
  if (count > MAX_SERIES) {
    fail({ code: "series_too_many", dataset: result.dataset, count }, [wholeDataset(result.dataset)]);
  }
  return groups;
}

/**
 * What one series is called, and its key. The label is the series value, the y column, or both
 * when there are several of each (the contract draws one series for every pair). The key is the
 * pair itself, so two series never share one whatever the values are (a value can contain `·`);
 * the legend hides a series by its key.
 */
function seriesNames(
  group: string,
  column: string,
  hasSeries: boolean,
  yCount: number,
): { key: string; label: string } {
  const key = JSON.stringify([group, column]);
  if (!hasSeries) return { key, label: column };
  return { key, label: yCount === 1 ? group : `${group} · ${column}` };
}

function timeSeriesPoints(
  result: DatasetResult,
  xColumn: string,
  x: number,
  y: number,
  rows: readonly number[],
): Point[] {
  const points = rows.map((row) => ({
    x: timeOf(result, xColumn, x, row),
    y: numberCell(cellAt(result, row, y)),
    row,
  }));
  points.sort((a, b) => a.x - b.x || a.row - b.row);
  for (let i = 1; i < points.length; i++) {
    const previous = points[i - 1] as Point;
    const current = points[i] as Point;
    if (previous.x === current.x) {
      fail({ code: "x_duplicate", dataset: result.dataset, column: xColumn, rows: [previous.row, current.row] }, [
        { kind: "rows", dataset: result.dataset, rows: [previous.row, current.row] },
      ]);
    }
  }
  return points;
}

function timeSeries(spec: TimeSeriesComponent, data: DashboardData): TimeSeriesView {
  const result = readyResult(data, spec.dataset);
  const x = typedColumn(result, spec.x, "time");
  const ys = spec.y.map((column) => ({ column, index: typedColumn(result, column, "number").index }));
  const hasSeries = spec.series !== undefined && spec.series !== null;
  const series: SeriesView[] = [];
  for (const [group, rows] of splitSeries(result, spec.series, ys.length)) {
    for (const y of ys) {
      series.push({
        ...seriesNames(group, y.column, hasSeries, ys.length),
        slot: series.length,
        column: y.column,
        points: timeSeriesPoints(result, spec.x, x.index, y.index, rows),
      });
    }
  }
  const xs = [...new Set(series.flatMap((line) => line.points.map((point) => point.x)))].sort((a, b) => a - b);
  return {
    kind: "time_series",
    title: spec.title,
    unit: spec.unit ?? undefined,
    dataset: spec.dataset,
    xColumn: spec.x,
    dateOnly: result.rows.length > 0 && result.rows.every((row) => isDateOnly(row[x.index] ?? null)),
    series,
    xs,
    thresholds: thresholdsOf(data, spec.thresholds),
  };
}

function bar(spec: BarComponent, data: DashboardData): BarView {
  const result = readyResult(data, spec.dataset);
  const x = typedColumn(result, spec.x, "category");
  const ys = spec.y.map((column) => ({ column, index: typedColumn(result, column, "number").index }));
  const hasSeries = spec.series !== undefined && spec.series !== null;
  const groups = splitSeries(result, spec.series, ys.length);
  // Categories in the order the query returned them; a category is one x cell.
  const categories: BarCategory[] = [];
  const categoryOf = new Map<string, number>();
  for (let row = 0; row < result.rows.length; row++) {
    const cell = cellAt(result, row, x.index);
    if (cell === null)
      fail({ code: "missing_x", dataset: spec.dataset, column: spec.x, row }, [wholeDataset(spec.dataset)]);
    const key = String(cell);
    if (categoryOf.has(key)) continue;
    categoryOf.set(key, categories.length);
    categories.push({ label: key, time: x.type === "time" ? timeOf(result, spec.x, x.index, row) : null });
  }
  const series: BarSeriesView[] = [];
  for (const [group, rows] of groups) {
    for (const y of ys) {
      const values: ({ y: number | null; row: number } | null)[] = categories.map(() => null);
      for (const row of rows) {
        const category = categoryOf.get(String(cellAt(result, row, x.index))) as number;
        const existing = values[category];
        if (existing) {
          fail({ code: "x_duplicate", dataset: spec.dataset, column: spec.x, rows: [existing.row, row] }, [
            { kind: "rows", dataset: spec.dataset, rows: [existing.row, row] },
          ]);
        }
        values[category] = { y: numberCell(cellAt(result, row, y.index)), row };
      }
      series.push({ ...seriesNames(group, y.column, hasSeries, ys.length), slot: series.length, values });
    }
  }
  return {
    kind: "bar",
    title: spec.title,
    unit: spec.unit ?? undefined,
    dataset: spec.dataset,
    xColumn: spec.x,
    xType: x.type === "time" ? "time" : "text",
    dateOnly: x.type === "time" && result.rows.every((row) => isDateOnly(row[x.index] ?? null)),
    categories,
    series,
    thresholds: thresholdsOf(data, spec.thresholds),
  };
}

/** The datasets a formula reads, in order of appearance. */
function formulaDatasets(expr: Expr, into: Set<string> = new Set()): Set<string> {
  switch (expr.kind) {
    case "number":
      return into;
    case "negate":
      return formulaDatasets(expr.inner, into);
    case "binary":
      formulaDatasets(expr.left, into);
      return formulaDatasets(expr.right, into);
    case "aggregate":
    case "index":
      into.add(expr.dataset);
      return into;
  }
}

/** The datasets a formula reads, in order of appearance; none when it does not parse. */
export function formulaDatasetNames(formula: string): string[] {
  const parsed = parseFormula(formula);
  return parsed.ok ? [...formulaDatasets(parsed.expr)] : [];
}

/** A computation's value, by the same formula code the sandbox runtime and the check use. */
function computationValue(data: DashboardData, name: string, unit: string | undefined): ResolvedValue {
  const computation = data.computations.find((candidate) => candidate.name === name);
  if (computation === undefined) fail({ code: "computation_unknown", name });
  const source: SourceRef = { kind: "computation", name };
  const parsed = parseFormula(computation.formula);
  if (!parsed.ok) fail({ code: "computation_invalid", name }, [source]);
  const results = new Map<string, DatasetResult>();
  for (const dataset of formulaDatasets(parsed.expr)) results.set(dataset, readyResult(data, dataset));
  const problem = checkFormula(parsed.expr, (dataset) => {
    const result = results.get(dataset);
    const declared = datasetOf(data, dataset);
    return result === undefined || declared === undefined
      ? null
      : {
          maxRows: declared.max_rows,
          columns: result.columns.map((column) => ({ name: column.name, type: column.type })),
        };
  });
  if (problem !== null) fail({ code: "computation_invalid", name }, [source]);
  const value = evaluateFormula(parsed.expr, (dataset, column) => {
    const result = results.get(dataset);
    const found = result === undefined ? undefined : columnOf(result, column);
    if (result === undefined || found === undefined) return null;
    return result.rows.map((row) => cellOf(row[found.index] ?? null));
  });
  return { value, label: computation.title, unit: unit ?? computation.unit ?? undefined, source };
}

function resolveValue(data: DashboardData, reference: ValueReference, unit: string | undefined): ResolvedValue {
  if (reference.kind === "computation") return computationValue(data, reference.name, unit);
  const declared = datasetOf(data, reference.dataset);
  if (declared === undefined) fail({ code: "dataset_unknown", dataset: reference.dataset });
  // The contract's limit, known from the spec alone: a row at or past `max_rows` is never run.
  if (reference.row >= declared.max_rows) {
    fail({ code: "row_out_of_range", dataset: reference.dataset, row: reference.row }, [
      wholeDataset(reference.dataset),
    ]);
  }
  const result = readyResult(data, reference.dataset);
  const column = typedColumn(result, reference.column, "number");
  if (reference.row >= result.rows.length) {
    fail({ code: "row_missing", dataset: reference.dataset, row: reference.row }, [wholeDataset(reference.dataset)]);
  }
  return {
    value: numberCell(cellAt(result, reference.row, column.index)),
    label: reference.column,
    unit,
    source: { kind: "rows", dataset: reference.dataset, rows: [reference.row] },
  };
}

function kpi(spec: KpiComponent, data: DashboardData): KpiView {
  const unit = spec.unit ?? undefined;
  const value = resolveValue(data, spec.value, unit);
  let trend: KpiView["trend"] = null;
  if (spec.trend != null) {
    const result = readyResult(data, spec.trend.dataset);
    const x = typedColumn(result, spec.trend.x, "time");
    const y = typedColumn(result, spec.trend.y, "number");
    const points = timeSeriesPoints(
      result,
      spec.trend.x,
      x.index,
      y.index,
      result.rows.map((_, row) => row),
    );
    trend = {
      dataset: spec.trend.dataset,
      series: { key: spec.trend.y, label: spec.trend.y, slot: 0, column: spec.trend.y, points },
      dateOnly: result.rows.length > 0 && result.rows.every((row) => isDateOnly(row[x.index] ?? null)),
    };
  }
  let comparison: KpiView["comparison"] = null;
  if (spec.compare != null) {
    // Its own unit: the component's `unit` is the value's (the comparison can be a ratio or
    // another quantity: the contract's sample compares kWh with kgCO2e).
    const compared = computationValue(data, spec.compare, undefined);
    comparison = { ...compared, title: compared.label };
  }
  return { kind: "kpi", title: spec.title, value, trend, comparison };
}

function table(spec: TableComponent, data: DashboardData): TableView {
  const result = readyResult(data, spec.dataset);
  const columns = spec.columns.map(({ column, unit }) => {
    const found = typedColumn(result, column, "any");
    return { name: column, type: found.type, unit: unit ?? undefined, index: found.index };
  });
  return { kind: "table", title: spec.title, dataset: spec.dataset, columns, result };
}

function status(spec: StatusComponent, data: DashboardData): StatusView {
  const value = resolveValue(data, spec.value, spec.unit ?? undefined);
  const thresholds = thresholdsOf(data, spec.thresholds);
  for (let i = 1; i < thresholds.length; i++) {
    if ((thresholds[i - 1] as Threshold).value >= (thresholds[i] as Threshold).value) {
      fail(
        { code: "thresholds_not_ascending", names: thresholds.map((threshold) => threshold.name) },
        thresholds.map((threshold) => ({ kind: "constant", name: threshold.name })),
      );
    }
  }
  const band =
    value.value === null ? null : thresholds.filter((threshold) => (value.value as number) >= threshold.value).length;
  return { kind: "status", title: spec.title, value, thresholds, band };
}

/** What `spec` draws over `data`, or why it cannot. */
export function componentModel(spec: PlatformComponent, data: DashboardData): ComponentModel {
  try {
    switch (spec.kind) {
      case "time_series":
        return { state: "ready", view: timeSeries(spec, data) };
      case "bar":
        return { state: "ready", view: bar(spec, data) };
      case "kpi":
        return { state: "ready", view: kpi(spec, data) };
      case "table":
        return { state: "ready", view: table(spec, data) };
      case "status":
        return { state: "ready", view: status(spec, data) };
    }
  } catch (error) {
    if (error instanceof Stop) return error.model;
    throw error;
  }
}
