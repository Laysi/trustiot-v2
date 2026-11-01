// What a component reads besides its spec: the version's datasets, computations and constants
// (types generated from api/openapi.json) and each dataset's result for this viewing. Loading the
// results is the dashboard page's job (Laysi/trustiot-v2#462); a component only draws what it is
// given.
import type { components } from "../api/schema.gen.ts";

export type Dataset = components["schemas"]["DashboardDataset"];
export type DatasetResult = components["schemas"]["DashboardDatasetResult"];
export type Computation = components["schemas"]["DashboardComputation"];
export type Constant = components["schemas"]["DashboardConstant"];
export type ColumnType = components["schemas"]["DashboardColumnType"];
export type Cell = DatasetResult["rows"][number][number];

/** One dataset's run for this viewing. */
export type DatasetState =
  | { status: "loading" }
  /** The run failed; `message` is already translated (the page owns the request and its error). */
  | { status: "failed"; message: string }
  | { status: "ready"; result: DatasetResult };

export interface DashboardData {
  datasets: readonly Dataset[];
  computations: readonly Computation[];
  constants: readonly Constant[];
  /** By dataset name; a dataset without an entry has not started loading. */
  results: Readonly<Record<string, DatasetState>>;
}

export function datasetOf(data: DashboardData, name: string): Dataset | undefined {
  return data.datasets.find((dataset) => dataset.name === name);
}

/**
 * A dataset's state; only the result's own entries count, so a dataset named `constructor` that
 * has not started loading is loading, not `Object.prototype.constructor`.
 */
export function stateOf(data: DashboardData, name: string): DatasetState {
  return (Object.hasOwn(data.results, name) ? data.results[name] : undefined) ?? { status: "loading" };
}

/** A column's index and type in a result, or `undefined` when the result has no such column. */
export function columnOf(result: DatasetResult, name: string): { index: number; type: ColumnType } | undefined {
  const index = result.columns.findIndex((column) => column.name === name);
  const column = result.columns[index];
  return column === undefined ? undefined : { index, type: column.type };
}

/** The cell at `row` and column `index`; `null` where the row is shorter. */
export function cellAt(result: DatasetResult, row: number, index: number): Cell {
  return result.rows[row]?.[index] ?? null;
}
