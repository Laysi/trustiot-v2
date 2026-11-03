import type { MessageId } from "../i18n/messages.ts";
import type { DashboardEnv } from "./context.tsx";
import type { DashboardData } from "./data.ts";
import type { ColumnExpectation, Problem } from "./model.ts";
import { MAX_SERIES } from "./spec.ts";

/** What the contract says for a column that is not of the type the component needs. */
const COLUMN_TYPE_MESSAGES: Readonly<Record<ColumnExpectation, MessageId>> = {
  time: "dashboard.spec.column_not_time",
  number: "dashboard.spec.column_not_number",
  text: "dashboard.spec.column_not_text",
  category: "dashboard.spec.column_not_category",
  // Only a column of a type this build does not know: nothing to show it with.
  any: "dashboard.spec.column_unknown",
};

/** A dataset as people know it: its title, falling back to its name. */
export function datasetTitle(data: DashboardData, name: string): string {
  return data.datasets.find((dataset) => dataset.name === name)?.title ?? name;
}

/**
 * Why a component shows no data, in the viewer's language. A problem the contract also finds at
 * write time (`SpecProblem`, spec.rs) has the contract's own message (`dashboard.spec.*`), so a
 * viewer reads the same words in the assistant's answer and on the dashboard; the ones only a
 * viewing can find have a message of their own (`dashboard.problem.*`). The card's buttons open
 * the dataset or the value the problem is about. Rows are counted from 1.
 */
export function problemMessage(problem: Problem, data: DashboardData, env: DashboardEnv): string {
  const { t } = env;
  switch (problem.code) {
    case "dataset_unknown":
      return t("dashboard.spec.dataset_unknown");
    case "column_unknown":
      return t("dashboard.spec.column_unknown");
    case "column_type":
      return t(COLUMN_TYPE_MESSAGES[problem.expected]);
    case "x_duplicate":
      return t("dashboard.spec.x_duplicate");
    case "series_too_many":
      return t("dashboard.spec.series_too_many", { max: MAX_SERIES });
    case "row_out_of_range":
      return t("dashboard.spec.row_out_of_range");
    case "row_missing":
      return t("dashboard.spec.row_missing");
    case "computation_unknown":
      return t("dashboard.spec.computation_unknown");
    case "constant_unknown":
      return t("dashboard.spec.constant_unknown");
    case "threshold_not_number":
      return t("dashboard.spec.threshold_not_number");
    case "thresholds_not_ascending":
      return t("dashboard.spec.thresholds_not_ascending");
    case "dataset_failed":
      return t("dashboard.problem.dataset_failed", {
        dataset: datasetTitle(data, problem.dataset),
        message: problem.message,
      });
    case "missing_x":
      return t("dashboard.problem.missing_x", {
        dataset: datasetTitle(data, problem.dataset),
        column: problem.column,
        row: problem.row + 1,
      });
    case "computation_invalid":
      return t("dashboard.problem.computation_invalid", {
        name: data.computations.find((computation) => computation.name === problem.name)?.title ?? problem.name,
      });
  }
}
