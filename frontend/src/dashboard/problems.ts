import type { MessageId } from "../i18n/messages.ts";
import type { MessageValues } from "../i18n/translate.ts";
import type { DashboardEnv } from "./context.tsx";
import type { ColumnType, DashboardData } from "./data.ts";
import type { Problem } from "./model.ts";
import { MAX_SERIES } from "./spec.ts";

const TYPE_NAMES: Readonly<Record<ColumnType, MessageId>> = {
  number: "dashboard.type.number",
  text: "dashboard.type.text",
  time: "dashboard.type.time",
  bool: "dashboard.type.bool",
};

/** A dataset as people know it: its title, falling back to its name. */
export function datasetTitle(data: DashboardData, name: string): string {
  return data.datasets.find((dataset) => dataset.name === name)?.title ?? name;
}

/** Why a component shows no data, in the viewer's language. Rows are counted from 1. */
export function problemMessage(problem: Problem, data: DashboardData, env: DashboardEnv): string {
  const { t } = env;
  const types = (list: readonly ColumnType[]) =>
    list.map((type) => t(TYPE_NAMES[type])).join(env.locale === "zh-TW" ? "或" : " or ");
  const dataset = "dataset" in problem ? datasetTitle(data, problem.dataset) : "";
  const message = (id: MessageId, values: MessageValues = {}) => t(id, { dataset, ...values });
  switch (problem.code) {
    case "dataset_unknown":
      return message("dashboard.problem.dataset_unknown", { dataset: problem.dataset });
    case "dataset_failed":
      return message("dashboard.problem.dataset_failed", { message: problem.message });
    case "column_missing":
      return message("dashboard.problem.column_missing", { column: problem.column });
    case "column_type":
      return message("dashboard.problem.column_type", {
        column: problem.column,
        actual: t(TYPE_NAMES[problem.actual]),
        expected: types(problem.expected),
      });
    case "duplicate_x":
      return message("dashboard.problem.duplicate_x", {
        column: problem.column,
        first: problem.rows[0] + 1,
        second: problem.rows[1] + 1,
      });
    case "missing_x":
      return message("dashboard.problem.missing_x", { column: problem.column, row: problem.row + 1 });
    case "too_many_series":
      return message("dashboard.problem.too_many_series", { count: problem.count, max: MAX_SERIES });
    case "series_needs_one_y":
      return message("dashboard.problem.series_needs_one_y");
    case "row_missing":
      return message("dashboard.problem.row_missing", { row: problem.row + 1 });
    case "computation_unknown":
      return t("dashboard.problem.computation_unknown", { name: problem.name });
    case "computation_invalid":
      return t("dashboard.problem.computation_invalid", {
        name: data.computations.find((computation) => computation.name === problem.name)?.title ?? problem.name,
      });
    case "constant_unknown":
      return t("dashboard.problem.constant_unknown", { name: problem.name });
    case "constant_not_number":
      return t("dashboard.problem.constant_not_number", {
        name: data.constants.find((constant) => constant.name === problem.name)?.title ?? problem.name,
      });
    case "thresholds_order":
      return t("dashboard.problem.thresholds_order");
  }
}
