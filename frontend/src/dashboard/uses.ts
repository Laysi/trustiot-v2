// What a component reads, from its spec alone: the datasets it needs (directly, through a
// computation's formula, or for a KPI's trend and comparison) and the source its card's header
// opens. The dashboard page runs the datasets of the components first, in the order they are
// shown; the card says when its data was queried.
import { type DashboardData, stateOf } from "./data.ts";
import { formulaDatasetNames, type SourceRef } from "./model.ts";
import type { PlatformComponent, ValueReference } from "./spec.ts";

function computationDatasets(data: Pick<DashboardData, "computations">, name: string): string[] {
  const computation = data.computations.find((candidate) => candidate.name === name);
  return computation === undefined ? [] : formulaDatasetNames(computation.formula);
}

function valueDatasets(data: Pick<DashboardData, "computations">, value: ValueReference): string[] {
  return value.kind === "cell" ? [value.dataset] : computationDatasets(data, value.name);
}

/** The datasets `spec` reads, in the order it reads them, each once. */
export function componentDatasets(spec: PlatformComponent, data: Pick<DashboardData, "computations">): string[] {
  let names: string[];
  switch (spec.kind) {
    case "time_series":
    case "bar":
    case "table":
      names = [spec.dataset];
      break;
    case "kpi":
      names = [
        ...valueDatasets(data, spec.value),
        ...(spec.trend != null ? [spec.trend.dataset] : []),
        ...(spec.compare != null ? computationDatasets(data, spec.compare) : []),
      ];
      break;
    case "status":
      names = valueDatasets(data, spec.value);
      break;
  }
  return [...new Set(names)];
}

/** The source a card's header opens: the whole dataset it draws, or the computation it shows. */
export function componentSource(spec: PlatformComponent): SourceRef {
  switch (spec.kind) {
    case "time_series":
    case "bar":
    case "table":
      return { kind: "rows", dataset: spec.dataset, rows: [] };
    case "kpi":
    case "status":
      return spec.value.kind === "cell"
        ? { kind: "rows", dataset: spec.value.dataset, rows: [] }
        : { kind: "computation", name: spec.value.name };
  }
}

/**
 * When the data a card shows was queried: the latest `queried_at` of the datasets it reads, once
 * every one of them has a result; `null` while one is loading or failed.
 */
export function componentQueriedAt(spec: PlatformComponent, data: DashboardData): string | null {
  let latest: { at: string; ms: number } | null = null;
  for (const name of componentDatasets(spec, data)) {
    const state = stateOf(data, name);
    if (state.status !== "ready") return null;
    const at = state.result.meta.queried_at;
    const ms = Date.parse(at);
    if (latest === null || ms > latest.ms) latest = { at, ms };
  }
  return latest?.at ?? null;
}
