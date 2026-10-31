import { describe, expect, test } from "bun:test";

import formulaVectors from "../../../crates/contract/tests/fixtures/dashboard/formula-vectors.json";
import {
  type Cell,
  cellOf,
  checkFormula,
  type DatasetShape,
  evaluateFormula,
  FORMULA_ERROR_CLASSES,
  type FormulaError,
  parseFormula,
  toSexpr,
} from "./formula.ts";

interface VectorDataset {
  max_rows: number;
  columns: { name: string; type: string }[];
  rows: unknown[][];
}

interface VectorCase {
  formula: string;
  ast?: unknown;
  value?: number | null;
  error?: string;
  position?: number;
}

const vectors = formulaVectors as unknown as { datasets: Record<string, VectorDataset>; cases: VectorCase[] };

function shape(name: string): DatasetShape | null {
  const dataset = vectors.datasets[name];
  return dataset === undefined ? null : { maxRows: dataset.max_rows, columns: dataset.columns };
}

function cells(dataset: string, column: string): Cell[] | null {
  const data = vectors.datasets[dataset];
  const index = data?.columns.findIndex((candidate) => candidate.name === column) ?? -1;
  if (data === undefined || index < 0) return null;
  return data.rows.map((row) => cellOf(row[index]));
}

/** Parse, then check against the vectors' datasets: the first problem, or the tree. */
function run(text: string): { error: FormulaError } | { ast: unknown; value: number | null } {
  const parsed = parseFormula(text);
  if (!parsed.ok) return { error: parsed.error };
  const problem = checkFormula(parsed.expr, shape);
  if (problem !== null) return { error: problem };
  return { ast: toSexpr(parsed.expr), value: evaluateFormula(parsed.expr, cells) };
}

describe("the shared formula vectors", () => {
  test("cover every error class and both outcomes", () => {
    const classes = new Set(vectors.cases.flatMap((vector) => (vector.error === undefined ? [] : [vector.error])));
    expect([...classes].sort()).toEqual([...FORMULA_ERROR_CLASSES].sort());
    expect(vectors.cases.filter((vector) => vector.ast !== undefined).length).toBeGreaterThan(40);
  });

  for (const vector of vectors.cases) {
    const label = JSON.stringify(vector.formula).slice(0, 60);
    test(`${label} ${vector.error === undefined ? "evaluates" : `fails with ${vector.error}`}`, () => {
      const outcome = run(vector.formula);
      if (vector.error !== undefined) {
        const expected: FormulaError =
          vector.position === undefined
            ? { class: vector.error as FormulaError["class"] }
            : { class: vector.error as FormulaError["class"], position: vector.position };
        expect(outcome).toEqual({ error: expected });
      } else {
        expect(outcome).toEqual({ ast: vector.ast, value: vector.value ?? null });
      }
    });
  }
});

describe("details the vectors leave out", () => {
  test("a formula of many multi-byte characters is too long before it is lexed", () => {
    // 171 three-byte characters are 513 bytes but only 171 characters.
    expect(parseFormula("用".repeat(171))).toEqual({ ok: false, error: { class: "too_long" } });
    expect(parseFormula("用".repeat(170))).toEqual({
      ok: false,
      error: { class: "unexpected_character", position: 1 },
    });
  });

  test("positions count code points, not UTF-16 units", () => {
    // U+1F600 is one character in Rust and two UTF-16 units in JavaScript.
    expect(parseFormula("1 + \u{1F600}")).toEqual({ ok: false, error: { class: "unexpected_character", position: 5 } });
    expect(parseFormula("\u{1F600}")).toEqual({ ok: false, error: { class: "unexpected_character", position: 1 } });
    expect(parseFormula("(1 + 2")).toEqual({ ok: false, error: { class: "unexpected_end", position: 7 } });
  });

  test("an index written as a large integer is out of range, one with a fraction is not an index", () => {
    expect(parseFormula("a.kwh[99999999999]")).toEqual({
      ok: false,
      error: { class: "index_out_of_range", position: 7 },
    });
    expect(parseFormula("a.kwh[2e3]")).toEqual({ ok: false, error: { class: "index_out_of_range", position: 7 } });
    expect(parseFormula("a.kwh[0001999]")).toMatchObject({ ok: true });
  });

  test("cells that are not finite numbers count only for count", () => {
    const parsed = parseFormula("count(x.v) + sum(x.v)");
    if (!parsed.ok) throw new Error("parse");
    const column: Cell[] = [cellOf(1), cellOf("text"), cellOf(true), cellOf(null), cellOf(2)];
    expect(evaluateFormula(parsed.expr, () => column)).toBe(4 + 3);
    expect(cellOf(Number.POSITIVE_INFINITY)).toBeNull();
  });

  test("a missing column has no value", () => {
    const parsed = parseFormula("count(x.v)");
    if (!parsed.ok) throw new Error("parse");
    expect(evaluateFormula(parsed.expr, () => null)).toBeNull();
  });

  test("min and max keep the first of equal values (0 and -0)", () => {
    const min = parseFormula("min(x.v)");
    const max = parseFormula("max(x.v)");
    if (!min.ok || !max.ok) throw new Error("parse");
    expect(
      Object.is(
        evaluateFormula(min.expr, () => [0, -0]),
        0,
      ),
    ).toBe(true);
    expect(
      Object.is(
        evaluateFormula(max.expr, () => [-0, 0]),
        -0,
      ),
    ).toBe(true);
  });
});
