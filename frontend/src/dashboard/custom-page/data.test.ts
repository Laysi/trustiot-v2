// What the custom page's frame gets from the dashboard's data (data.ts), and how a clicked value
// maps back to its source.
import { describe, expect, test } from "bun:test";

import { PublicSpecSchema } from "../../../sandbox/src/protocol.ts";
import type { DashboardData, DatasetState } from "../data.ts";
import { fixtureData } from "../fixtures.ts";
import { DATASET_FAILED_KEY, deliveryOf, publicSpecOf, sourceOf } from "./data.ts";

/** `data` with one dataset's state replaced. */
function withResult(data: DashboardData, dataset: string, state: DatasetState): DashboardData {
  return { ...data, results: { ...data.results, [dataset]: state } };
}

describe("publicSpecOf", () => {
  test("carries no SQL or semantic request, and parses as the protocol's public spec", () => {
    const spec = publicSpecOf(fixtureData());
    expect(PublicSpecSchema.safeParse(spec).success).toBe(true);
    expect(JSON.stringify(spec)).not.toContain("SELECT");
    expect(spec.datasets[0]).toEqual({
      name: "power",
      title: "Line power",
      description: "Line power（測試資料）",
      source: "sql",
      max_rows: 2000,
    });
  });

  test("leaves out a null unit or note", () => {
    const data: DashboardData = {
      ...fixtureData(),
      computations: [{ name: "x", title: "X", formula: "1", unit: null }],
      constants: [{ name: "k", title: "K", value: 1, unit: null, note: null }],
    };
    const spec = publicSpecOf(data);
    expect(spec.computations[0]).toEqual({ name: "x", title: "X", formula: "1" });
    expect(spec.constants[0]).toEqual({ name: "k", title: "K", value: 1 });
  });
});

describe("deliveryOf", () => {
  test("null while a dataset is loading", () => {
    const data = withResult(fixtureData(), "daily", { status: "loading" });
    expect(deliveryOf(data)).toBeNull();
  });

  test("every result in the spec's order, and a failure as its message key", () => {
    const data = withResult(fixtureData(), "daily", { status: "failed", message: "Timed out" });
    const delivered = deliveryOf(data);
    expect(delivered?.map((item) => item.status)).toEqual(["ok", "error", "ok", "ok"]);
    // The outer page's message stays with it.
    expect(delivered?.[1]).toEqual({ status: "error", dataset: "daily", message_key: DATASET_FAILED_KEY });
  });
});

describe("sourceOf", () => {
  const data: DashboardData = fixtureData();

  test("a cell of a row in the result", () => {
    expect(sourceOf("c|power|kw|1", data)).toEqual({ kind: "rows", dataset: "power", rows: [1] });
    expect(sourceOf("c|power|kw|7", data)).toBeNull();
    expect(sourceOf("c|nope|kw|0", data)).toBeNull();
  });

  test("a dataset's meta field or a tick: the dataset", () => {
    expect(sourceOf("m|daily|queried_at", data)).toEqual({ kind: "rows", dataset: "daily", rows: [] });
    expect(sourceOf("t|trend|kwh|3", data)).toEqual({ kind: "rows", dataset: "trend", rows: [] });
  });

  test("a computation or constant of this version", () => {
    expect(sourceOf("f|total_kwh", data)).toEqual({ kind: "computation", name: "total_kwh" });
    expect(sourceOf("k|limit", data)).toEqual({ kind: "constant", name: "limit" });
    expect(sourceOf("f|nope", data)).toBeNull();
    expect(sourceOf("k|nope", data)).toBeNull();
  });

  test("anything that is not a reference", () => {
    for (const text of ["", "x", "c|power|kw", "f|", "<img src=x>", "c|power|kw|-1", "k|__proto__|x"]) {
      expect(sourceOf(text, data)).toBeNull();
    }
  });

  test("a cell of a dataset that did not load", () => {
    const failed = withResult(fixtureData(), "power", { status: "failed", message: "x" });
    expect(sourceOf("c|power|kw|0", failed)).toBeNull();
  });
});
