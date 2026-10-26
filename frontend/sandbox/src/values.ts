// What a reference resolves to (frontend/sandbox/README.md, 「參照」): the TypeScript twin of
// `CheckRequest::resolves`, `value_of` and `declares` in crates/contract/src/api/dashboard/check.rs.
// The runtime answers `tq.value` with it, and the comparison judges every mark with it, from data
// the caller gives and never from the page.
//
// A reference has two values: the contract value, which a passing verdict reports and the
// assistant checks against its own data (a cell as the result holds it, a time as its text), and
// the display value a page formats (a time as a wall-clock `Date`, see time.ts).

import type { DisplayValue } from "./format.ts";
import { type Cell, cellOf, evaluateFormula, parseFormula } from "./formula.ts";
import type { DatasetResult, PublicSpec, Reference } from "./protocol.ts";
import { MAX_DATASET_ROWS } from "./protocol.ts";
import { instantToWall, parseWallTime, tickTimeDate } from "./time.ts";

/** A value in a verdict: as JSON carries it. */
export type ContractValue = number | string | boolean | null;

export interface Resolved {
  value: ContractValue;
  display: DisplayValue;
}

/** The data references resolve against: the spec's public part and the results that arrived. */
export interface Model {
  spec: PublicSpec;
  /** One result per dataset that ran; a dataset that failed has none. */
  results: readonly DatasetResult[];
  /** The viewer's time zone, for meta times. */
  tz: string;
}

export function resultOf(model: Model, dataset: string): DatasetResult | undefined {
  for (let i = 0; i < model.results.length; i++) {
    const result = model.results[i];
    if (result !== undefined && result.dataset === dataset) return result;
  }
  return undefined;
}

function columnIndex(result: DatasetResult, column: string): number {
  for (let i = 0; i < result.columns.length; i++) {
    if (result.columns[i]?.name === column) return i;
  }
  return -1;
}

/** A column's type in a result, or `undefined` when it has no such column. */
export function columnType(result: DatasetResult, column: string): string | undefined {
  const index = columnIndex(result, column);
  return index < 0 ? undefined : result.columns[index]?.type;
}

/** The raw cells of a column, in row order; `null` when there is no such column. */
export function columnValues(result: DatasetResult, column: string): unknown[] | null {
  const index = columnIndex(result, column);
  if (index < 0) return null;
  const values: unknown[] = [];
  for (let row = 0; row < result.rows.length; row++) values[row] = result.rows[row]?.[index] ?? null;
  return values;
}

/** A cell as a page sees it, by its column's type. */
export function cellDisplay(type: string | undefined, cell: unknown): DisplayValue {
  if (cell === null || cell === undefined) return null;
  if (type === "time" && typeof cell === "string") return parseWallTime(cell) ?? cell;
  if (typeof cell === "number" || typeof cell === "string" || typeof cell === "boolean") return cell;
  return null;
}

/** Whether `reference` resolves to a value in the data (`CheckRequest::resolves`). */
export function resolves(model: Model, reference: Reference): boolean {
  switch (reference.form) {
    case "f":
      return model.spec.computations.some((computation) => computation.name === reference.name);
    case "k":
      return model.spec.constants.some((constant) => constant.name === reference.name);
    case "m": {
      const result = resultOf(model, reference.dataset);
      if (result === undefined) return false;
      return reference.field === "queried_at" || reference.field === "row_count" || result.meta.source.kind !== "sql";
    }
    case "c": {
      const result = resultOf(model, reference.dataset);
      return (
        result !== undefined &&
        columnIndex(result, reference.column) >= 0 &&
        reference.row < Math.min(result.row_count, MAX_DATASET_ROWS)
      );
    }
    case "t": {
      const result = resultOf(model, reference.dataset);
      return result !== undefined && columnIndex(result, reference.column) >= 0;
    }
  }
}

/**
 * Whether `reference` names only what the data declares, whether or not it resolves
 * (`CheckRequest::declares`): the rule for naming a reference in a problem.
 */
export function declares(model: Model, reference: Reference): boolean {
  switch (reference.form) {
    case "f":
    case "k":
      return resolves(model, reference);
    case "m":
      return model.spec.datasets.some((dataset) => dataset.name === reference.dataset);
    case "c":
    case "t": {
      const result = resultOf(model, reference.dataset);
      return result !== undefined && columnIndex(result, reference.column) >= 0;
    }
  }
}

/** A computation's value over the results; `undefined` when its formula does not parse. */
export function computationValue(model: Model, name: string): number | null | undefined {
  const computation = model.spec.computations.find((candidate) => candidate.name === name);
  if (computation === undefined) return undefined;
  const parsed = parseFormula(computation.formula);
  if (!parsed.ok) return undefined;
  return evaluateFormula(parsed.expr, (dataset, column) => {
    const result = resultOf(model, dataset);
    const values = result === undefined ? null : columnValues(result, column);
    if (values === null) return null;
    const cells: Cell[] = [];
    for (let i = 0; i < values.length; i++) cells[i] = cellOf(values[i]);
    return cells;
  });
}

/**
 * The value `reference` resolves to (`CheckRequest::value_of`) and how a page sees it; `null`
 * when it does not resolve.
 */
export function resolve(model: Model, reference: Reference): Resolved | null {
  if (!resolves(model, reference)) return null;
  switch (reference.form) {
    case "c": {
      const result = resultOf(model, reference.dataset);
      if (result === undefined) return null;
      const index = columnIndex(result, reference.column);
      const row = result.rows[reference.row];
      if (row === undefined || index < 0 || index >= row.length) return null;
      const cell = row[index] ?? null;
      return { value: cell, display: cellDisplay(result.columns[index]?.type, cell) };
    }
    case "f": {
      const value = computationValue(model, reference.name);
      return value === undefined ? null : { value, display: value };
    }
    case "k": {
      const constant = model.spec.constants.find((candidate) => candidate.name === reference.name);
      return constant === undefined ? null : { value: constant.value, display: constant.value };
    }
    case "m": {
      const result = resultOf(model, reference.dataset);
      if (result === undefined) return null;
      if (reference.field === "row_count") return { value: result.row_count, display: result.row_count };
      const source = result.meta.source;
      const text =
        reference.field === "queried_at"
          ? result.meta.queried_at
          : source.kind === "sql"
            ? undefined
            : reference.field === "range_from"
              ? source.range.from
              : source.range.to;
      if (text === undefined) return null;
      return { value: text, display: instantToWall(text, model.tz) ?? text };
    }
    case "t":
      return typeof reference.value === "number"
        ? { value: reference.value, display: reference.value }
        : { value: reference.value, display: tickTimeDate(reference.value) };
  }
}
