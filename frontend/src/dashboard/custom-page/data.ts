// What the custom page's frame gets from the dashboard's data, and how a value the user clicked in
// it maps back to its source. The page gets the public part of the spec (no SQL, no semantic
// request: `publicSpec`) and the results; the source panel shows the rest from the outer page's own
// copy, never from anything the frame sent beyond a reference that resolves.
import type { DeliveredDataset } from "../../../sandbox/src/host.ts";
import { MAX_DATASET_ROWS, type PublicSpec, parseReference } from "../../../sandbox/src/protocol.ts";
import { type DashboardData, datasetOf, stateOf } from "../data.ts";
import type { SourceRef } from "../model.ts";

/**
 * The message key a page reads (`tq.dataset(name).messageKey`) for a dataset whose run failed: the
 * outer page has the translated reason, which it shows itself, not a key.
 */
export const DATASET_FAILED_KEY = "dashboard.dataset.failed";

/** The spec's public part, as the sandbox protocol carries it (`publicSpec`). */
export function publicSpecOf(data: DashboardData): PublicSpec {
  return {
    datasets: data.datasets.map((dataset) => ({
      name: dataset.name,
      title: dataset.title,
      description: dataset.description,
      source: dataset.source.kind,
      max_rows: dataset.max_rows,
    })),
    computations: data.computations.map((computation) => ({
      name: computation.name,
      title: computation.title,
      formula: computation.formula,
      ...(computation.unit == null ? {} : { unit: computation.unit }),
    })),
    constants: data.constants.map((constant) => ({
      name: constant.name,
      title: constant.title,
      value: constant.value,
      ...(constant.unit == null ? {} : { unit: constant.unit }),
      ...(constant.note == null ? {} : { note: constant.note }),
    })),
  };
}

/**
 * Every dataset's result or failure, once none is loading any more; `null` while one still is. The
 * sandbox takes all of them at once (`data`).
 */
export function deliveryOf(data: DashboardData): DeliveredDataset[] | null {
  const delivered: DeliveredDataset[] = [];
  for (const dataset of data.datasets) {
    const state = stateOf(data, dataset.name);
    if (state.status === "loading") return null;
    delivered.push(
      state.status === "ready"
        ? { status: "ok", result: state.result }
        : { status: "error", dataset: dataset.name, message_key: DATASET_FAILED_KEY },
    );
  }
  return delivered;
}

/**
 * Where a clicked value comes from, for the source panel: `null` unless `reference` is one and
 * names a dataset, computation or constant of this version (and, for a cell, a row of its result).
 */
export function sourceOf(reference: string, data: DashboardData): SourceRef | null {
  const parsed = parseReference(reference);
  if (parsed === null) return null;
  switch (parsed.form) {
    case "c": {
      if (datasetOf(data, parsed.dataset) === undefined) return null;
      const state = stateOf(data, parsed.dataset);
      if (state.status !== "ready" || parsed.row >= Math.min(state.result.rows.length, MAX_DATASET_ROWS)) return null;
      return { kind: "rows", dataset: parsed.dataset, rows: [parsed.row] };
    }
    case "m":
    case "t":
      return datasetOf(data, parsed.dataset) === undefined ? null : { kind: "rows", dataset: parsed.dataset, rows: [] };
    case "f":
      return data.computations.some((computation) => computation.name === parsed.name)
        ? { kind: "computation", name: parsed.name }
        : null;
    case "k":
      return data.constants.some((constant) => constant.name === parsed.name)
        ? { kind: "constant", name: parsed.name }
        : null;
  }
}
