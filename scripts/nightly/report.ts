// The result of one nightly run: written as JSON for the notification step and as Markdown
// for the job summary, the pull request comment and the failure issue.

export interface Check {
  name: string;
  ok: boolean;
  /** What was checked or why it failed. */
  detail: string;
  durationMs: number;
}

export interface ShapeResult {
  /** The values file's shape name: `saas` for values-saas.yaml. */
  shape: string;
  ok: boolean;
  checks: Check[];
  /** Cluster state and pod logs, collected when a check failed. */
  diagnostics?: string;
  durationMs: number;
}

export interface Report {
  startedAt: string;
  finishedAt: string;
  /** The k3s image of the cluster, with its digest. */
  k3sImage: string;
  /** Set when the cluster itself could not be created or torn down. */
  infraError?: string;
  shapes: ShapeResult[];
  ok: boolean;
}

export function isOk(report: Pick<Report, "infraError" | "shapes">, expectedShapes: number): boolean {
  return (
    report.infraError === undefined &&
    report.shapes.length === expectedShapes &&
    report.shapes.every((shape) => shape.ok)
  );
}

export function formatDuration(ms: number): string {
  const seconds = Math.round(ms / 1000);
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m${String(seconds % 60).padStart(2, "0")}s`;
}

/** Table cells must not contain pipes or line breaks. */
function cell(text: string): string {
  return text.replace(/\|/g, "\\|").replace(/\r?\n/g, " ");
}

/** Keeps the end of a long text, where the error usually is. */
export function tail(text: string, maxChars: number): string {
  if (text.length <= maxChars) return text;
  return `... (${text.length - maxChars} characters cut)\n${text.slice(text.length - maxChars)}`;
}

/** Markdown for a human: one table per values file, the diagnostics of failed ones below. */
export function renderMarkdown(report: Report, maxDiagnosticChars = 12_000): string {
  const lines: string[] = [];
  lines.push(`## nightly smoke test: ${report.ok ? "passed" : "FAILED"}`);
  lines.push("");
  lines.push(`- started ${report.startedAt}, finished ${report.finishedAt}`);
  lines.push(`- cluster: k3d with \`${report.k3sImage}\``);
  if (report.infraError !== undefined) {
    lines.push("");
    lines.push("**The cluster could not be set up or torn down:**");
    lines.push("");
    lines.push("```");
    lines.push(tail(report.infraError, maxDiagnosticChars));
    lines.push("```");
  }
  for (const shape of report.shapes) {
    lines.push("");
    lines.push(`### values-${shape.shape}.yaml: ${shape.ok ? "passed" : "FAILED"} (${formatDuration(shape.durationMs)})`);
    lines.push("");
    lines.push("| check | result | duration | detail |");
    lines.push("| --- | --- | --- | --- |");
    for (const check of shape.checks) {
      lines.push(
        `| ${cell(check.name)} | ${check.ok ? "ok" : "FAILED"} | ${formatDuration(check.durationMs)} | ${cell(tail(check.detail, 400))} |`,
      );
    }
    if (shape.diagnostics !== undefined && !shape.ok) {
      lines.push("");
      lines.push("<details><summary>diagnostics</summary>");
      lines.push("");
      lines.push("```");
      lines.push(tail(shape.diagnostics, maxDiagnosticChars));
      lines.push("```");
      lines.push("");
      lines.push("</details>");
    }
  }
  return `${lines.join("\n")}\n`;
}

/** The first failed check of each failed shape, for a one-line summary. */
export function failureHeadline(report: Report): string {
  if (report.infraError !== undefined) return `cluster setup or teardown failed: ${report.infraError.split("\n")[0]}`;
  const parts: string[] = [];
  for (const shape of report.shapes) {
    const failed = shape.checks.find((check) => !check.ok);
    if (failed !== undefined) parts.push(`values-${shape.shape}.yaml: ${failed.name}`);
  }
  return parts.length > 0 ? parts.join("; ") : "the smoke test did not finish";
}
