// Pending Laysi/trustiot-v2#153: replace with a module over the generated client when api/openapi.json has these endpoints.

/**
 * The platform administrator's system prompt API as the page needs it (docs/design/platform-admin/,
 * `ai-prompts.html` and `ai-prompt-versions.html`; rules in Laysi/trustiot-v2#153 and the checklist's
 * J8 row): one prompt per LLM role, kept by the service that uses it (the AI assistant now, alerts
 * and the knowledge base later, each listed as another service), every change a new version, and
 * a change only applies to new conversations.
 *
 * Assumptions, none of which the backend issue states yet: the endpoints are reached per service
 * and role; `variables` is the list of names the service accepts in `{{name}}`; a publish names
 * the version the editor started from (`expected_version`, 409 when another one is current); an
 * unknown variable is refused with 422 and the field error `/content` (the page checks the
 * variables itself first, so the refusal only shows when the lists differ); a revert needs a
 * reason and a publish may carry a note (decision Q9); `default_update` carries the text of the old
 * and the new default so the page can show their difference.
 */

/** Where a version came from: an edit, a revert to an older version, or the service's new default. */
export type PromptOrigin = "default" | "edit" | "revert" | "apply_default";

export interface PromptSummary {
  service: string;
  role: string;
  /** `default` until someone publishes a change. */
  state: "default" | "modified";
  /** The service shipped a newer default than the one this prompt was last based on (only a modified prompt can be behind). */
  default_update_pending: boolean;
  /** The role is on hold (the router, 2026-11-03). */
  on_hold: boolean;
  version: number;
  updated_at: string;
}

export interface PromptServiceGroup {
  service: string;
  prompts: PromptSummary[];
}

/** A part of the prompt that is code (J5's guardrails): shown, never editable. */
export interface FixedSection {
  id: string;
  title: string;
  content: string;
}

/** The default changing from one release to the next, with both texts. */
export interface DefaultUpdate {
  from_label: string;
  to_label: string;
  from_content: string;
  to_content: string;
}

export interface PromptDetail {
  service: string;
  role: string;
  version: number;
  content: string;
  updated_at: string;
  /** Who published it: an email for a platform administrator's lookup (decision Q14). */
  updated_by: string;
  fixed_sections: FixedSection[];
  /** The variable names the service accepts, without the braces. */
  variables: string[];
  /** The label of the default this prompt is based on, such as `2026.10`. */
  default_label: string;
  default_update: DefaultUpdate | null;
}

export interface PromptVersion {
  version: number;
  origin: PromptOrigin;
  /** The version a revert took its content from, or the default's label for `apply_default`. */
  source: string | null;
  /** The evaluation ran on this content before it was published. */
  evaluated: boolean;
  created_by: string;
  created_at: string;
  note: string | null;
}

export interface PromptVersionPage {
  versions: PromptVersion[];
  next_cursor?: string;
}

export interface PromptVersionContent {
  version: number;
  content: string;
}

export interface PublishRequest {
  content: string;
  note?: string;
  /** The version the draft was edited from. */
  expected_version: number;
}

export interface RevertRequest {
  to_version: number;
  reason: string;
  expected_version: number;
}

export interface EvalSubset {
  id: string;
  label: string;
  question_count: number;
}

export interface EvalOptions {
  subsets: EvalSubset[];
  runs_per_question: number;
  /** What one question costs, from the published version's last report (US dollars); `null` when there was none. */
  estimated_cost_per_question: number | null;
}

export interface EvalMetrics {
  accuracy: number;
  consistency: number;
  cost_per_correct: number;
  latency_p50: number;
  latency_p95: number;
}

export interface EvalReport {
  question_count: number;
  runs_per_question: number;
  finished_at: string;
  /** The published version's metrics (from its last report) and the draft's. */
  baseline: EvalMetrics;
  candidate: EvalMetrics;
  newly_wrong: { id: string; text: string }[];
}

export interface EvalRun {
  id: string;
  /** The text being evaluated: a draft, so the page can tell whether a report is about the draft it shows. */
  content: string;
  state: "running" | "done" | "cancelled" | "failed";
  completed: number;
  total: number;
  report: EvalReport | null;
}

/**
 * The prompt endpoints. A publish or revert names the version it started from: another current
 * version answers 409 `conflict`; an evaluation of the same prompt while one runs answers 409 too
 * (decision Q10: one at a time per prompt); the evaluation's cost is the platform's, not a tenant's.
 */
export interface PromptApi {
  listPrompts(signal?: AbortSignal): Promise<PromptServiceGroup[]>;
  getPrompt(service: string, role: string, signal?: AbortSignal): Promise<PromptDetail>;
  listVersions(service: string, role: string, cursor?: string, signal?: AbortSignal): Promise<PromptVersionPage>;
  getVersion(service: string, role: string, version: number, signal?: AbortSignal): Promise<PromptVersionContent>;
  publish(service: string, role: string, request: PublishRequest): Promise<PromptDetail>;
  revert(service: string, role: string, request: RevertRequest): Promise<PromptDetail>;
  /** Publishes the service's new default as the next version (the "use the new default" choice). */
  adoptDefault(service: string, role: string, expectedVersion: number): Promise<PromptDetail>;
  evalOptions(service: string, role: string, signal?: AbortSignal): Promise<EvalOptions>;
  /** The evaluation running for this prompt now, so a page opened again keeps showing its progress; `null` when none. */
  currentEval(service: string, role: string, signal?: AbortSignal): Promise<EvalRun | null>;
  startEval(service: string, role: string, request: { content: string; subsets: string[] }): Promise<EvalRun>;
  getEval(service: string, role: string, id: string, signal?: AbortSignal): Promise<EvalRun>;
  cancelEval(service: string, role: string, id: string): Promise<EvalRun>;
}
