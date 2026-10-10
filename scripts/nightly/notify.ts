// Reports the result of a nightly run with the workflow's own GITHUB_TOKEN, so no external
// service and no extra secret is involved (docs/architecture/ci-release.md, nightly.yml):
//
// - schedule and workflow_dispatch, failed: opens an issue labelled `nightly-failure`, or adds a
//   comment to the one that is already open, so a failing week is one issue, not seven.
// - schedule and workflow_dispatch, passed: when such an issue is open, comments that the run
//   recovered and closes it.
//
// This script runs only for scheduled and manual runs, which GitHub always starts from the
// default branch, so the code that holds the write token is reviewed code. Pull requests get
// their report comment from the unprivileged-code job `pr-comment` of nightly.yml instead.
//
// Environment: GITHUB_TOKEN, GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_SERVER_URL, GITHUB_API_URL,
// GITHUB_EVENT_NAME, GITHUB_SHA, NIGHTLY_RESULT (the smoke job's result), NIGHTLY_REPORT_DIR
// (the downloaded artifact), NIGHTLY_JOB_NAME.
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { failureHeadline, tail, type Report } from "./report.ts";

export const FAILURE_LABEL = "nightly-failure";
export const ISSUE_TITLE = "nightly 冒煙測試失敗：chart 在 k3d 裝不起來或起不來";
const COMMENT_LIMIT = 60_000;

export type Action = "failure" | "recovery" | "nothing";

/** What a run's event and result call for. Anything but `success` counts as a failure. */
export function decide(eventName: string, result: string): Action {
  if (eventName === "schedule" || eventName === "workflow_dispatch") return result === "success" ? "recovery" : "failure";
  return "nothing";
}

export interface RunInfo {
  runUrl: string;
  sha: string;
  eventName: string;
  result: string;
  /** Names of the failed steps of the smoke job, when the job ran at all. */
  failedSteps: string[];
}

export function loadReport(dir: string): { report?: Report; markdown?: string } {
  const json = join(dir, "report.json");
  const md = join(dir, "report.md");
  if (!existsSync(json) || !existsSync(md)) return {};
  try {
    return { report: JSON.parse(readFileSync(json, "utf8")) as Report, markdown: readFileSync(md, "utf8") };
  } catch {
    return {};
  }
}

function dateOf(report: Report | undefined, now: Date): string {
  return (report?.finishedAt ?? now.toISOString()).slice(0, 10);
}

/** Why there is no report: the job failed before or while writing one. */
function missingReportNote(info: RunInfo): string {
  const steps = info.failedSteps.length > 0 ? `失敗的步驟：${info.failedSteps.join("、")}。` : "";
  return `這次執行沒有產生報告（smoke job 的結果：${info.result}）。${steps}請到 workflow run 的日誌查看。`;
}

export function failureIssueBody(info: RunInfo, report: Report | undefined, markdown: string | undefined, now = new Date()): string {
  const date = dateOf(report, now);
  const headline = report === undefined ? `smoke job 的結果是 ${info.result}` : failureHeadline(report);
  return [
    "## 背景 (Why)",
    "",
    `nightly workflow 在 ${date} 的執行失敗：${headline}。nightly 每晚以 k3d 部署整套 chart（SaaS 與地端兩份 values 各一次）並跑冒煙測試；失敗代表其中一種部署型態目前裝不起來或起不來。此 issue 由 workflow 自動開立（來源：Laysi/trustiot-v2#69）。`,
    "",
    "## 範圍 (What)",
    "",
    "### 包含",
    "",
    "- 找出失敗原因並修正（chart、values、映像或 workflow）。",
    "",
    "### 不包含",
    "",
    "- 新增冒煙測試的檢查項目。",
    "",
    "## 現狀 (State)",
    "",
    `${date} 首次失敗，見下方 run。之後每晚仍失敗會在此 issue 留言；通過時 workflow 會留言並關閉此 issue。`,
    "",
    "## 驗收條件 (Acceptance Criteria)",
    "",
    "- [ ] 下一次 nightly 兩份 values 都通過。",
    "- [ ] 根因記錄在此 issue。",
    "",
    "## 指標 (Pointers)",
    "",
    `- Workflow run：${info.runUrl}（commit ${info.sha.slice(0, 12)}，事件 ${info.eventName}）`,
    "- 文件：docs/architecture/ci-release.md 的 nightly.yml 段、deploy/chart/README.md",
    "- 重跑：Actions 頁的 nightly workflow 可手動執行（workflow_dispatch）",
    "",
    "### 報告",
    "",
    markdown === undefined ? missingReportNote(info) : tail(markdown, COMMENT_LIMIT - 4_000),
    "",
    "<!-- 由 scripts/nightly/notify.ts 產生 -->",
  ].join("\n");
}

export function failureComment(info: RunInfo, report: Report | undefined, markdown: string | undefined, now = new Date()): string {
  const date = dateOf(report, now);
  return [
    `${date} 的 nightly 仍然失敗：${report === undefined ? `smoke job 的結果是 ${info.result}` : failureHeadline(report)}。`,
    "",
    `- Workflow run：${info.runUrl}（commit ${info.sha.slice(0, 12)}，事件 ${info.eventName}）`,
    "",
    markdown === undefined ? missingReportNote(info) : tail(markdown, COMMENT_LIMIT - 2_000),
  ].join("\n");
}

export function recoveryComment(info: RunInfo, now = new Date()): string {
  return `${now.toISOString().slice(0, 10)} 的 nightly 兩份 values 都通過，關閉此 issue。Workflow run：${info.runUrl}（commit ${info.sha.slice(0, 12)}）。`;
}

/** A small GitHub REST client: only what this script needs. */
export class GitHub {
  constructor(
    private readonly apiUrl: string,
    private readonly token: string,
    private readonly repository: string,
  ) {}

  async request<T>(method: string, path: string, body?: unknown, allowed: number[] = []): Promise<{ status: number; data: T }> {
    const response = await fetch(`${this.apiUrl}/repos/${this.repository}${path}`, {
      method,
      headers: {
        authorization: `Bearer ${this.token}`,
        accept: "application/vnd.github+json",
        "x-github-api-version": "2022-11-28",
        "content-type": "application/json",
        "user-agent": "trustiot-nightly",
      },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    if (!response.ok && !allowed.includes(response.status)) {
      throw new Error(`${method} ${path} failed with ${response.status}: ${text.slice(0, 300)}`);
    }
    return { status: response.status, data: (text === "" ? undefined : JSON.parse(text)) as T };
  }
}

interface IssueSummary {
  number: number;
  pull_request?: unknown;
}

async function openFailureIssue(github: GitHub): Promise<number | undefined> {
  const { data } = await github.request<IssueSummary[]>(
    "GET",
    `/issues?labels=${FAILURE_LABEL}&state=open&per_page=20&sort=created&direction=asc`,
  );
  return data.find((issue) => issue.pull_request === undefined)?.number;
}

async function ensureLabel(github: GitHub): Promise<void> {
  // 422 means the label exists already.
  await github.request("POST", "/labels", {
    name: FAILURE_LABEL,
    color: "b60205",
    description: "Opened by the nightly workflow when the smoke test fails (scripts/nightly/notify.ts)",
  }, [422]);
}

async function failedStepsOf(github: GitHub, runId: string, jobName: string): Promise<string[]> {
  try {
    const { data } = await github.request<{ jobs: { name: string; steps?: { name: string; conclusion: string | null }[] }[] }>(
      "GET",
      `/actions/runs/${runId}/jobs?per_page=50`,
    );
    const job = data.jobs.find((candidate) => candidate.name === jobName);
    return (job?.steps ?? []).filter((step) => step.conclusion === "failure" || step.conclusion === "cancelled").map((step) => step.name);
  } catch {
    return []; // the note about the missing report is the only place this is used
  }
}

export async function notify(github: GitHub, env: NodeJS.ProcessEnv, now = new Date()): Promise<string> {
  const eventName = env.GITHUB_EVENT_NAME ?? "";
  const result = env.NIGHTLY_RESULT ?? "";
  const runId = env.GITHUB_RUN_ID ?? "";
  const info: RunInfo = {
    runUrl: `${env.GITHUB_SERVER_URL ?? "https://github.com"}/${env.GITHUB_REPOSITORY}/actions/runs/${runId}`,
    sha: env.GITHUB_SHA ?? "",
    eventName,
    result,
    failedSteps: result === "success" ? [] : await failedStepsOf(github, runId, env.NIGHTLY_JOB_NAME ?? "smoke"),
  };
  const { report, markdown } = loadReport(env.NIGHTLY_REPORT_DIR ?? "nightly-report");
  const action = decide(eventName, result);

  if (action === "failure") {
    await ensureLabel(github);
    const open = await openFailureIssue(github);
    if (open !== undefined) {
      await github.request("POST", `/issues/${open}/comments`, { body: failureComment(info, report, markdown, now) });
      return `commented on issue #${open}`;
    }
    const issue = {
      title: ISSUE_TITLE,
      body: failureIssueBody(info, report, markdown, now),
      labels: [FAILURE_LABEL, "area:ci", "priority:P1"],
    };
    const created = await github.request<{ number: number }>("POST", "/issues", { ...issue, type: "Bug" }, [422]);
    if (created.status === 422) {
      // The repository has no `Bug` issue type: open the issue without one.
      const plain = await github.request<{ number: number }>("POST", "/issues", issue);
      return `opened issue #${plain.data.number}`;
    }
    return `opened issue #${created.data.number}`;
  }

  if (action === "recovery") {
    const open = await openFailureIssue(github);
    if (open === undefined) return "nothing to report";
    await github.request("POST", `/issues/${open}/comments`, { body: recoveryComment(info, now) });
    await github.request("PATCH", `/issues/${open}`, { state: "closed", state_reason: "completed" });
    return `closed issue #${open}`;
  }
  return "nothing to report";
}

if (import.meta.main) {
  const token = process.env.GITHUB_TOKEN;
  const repository = process.env.GITHUB_REPOSITORY;
  if (token === undefined || token === "" || repository === undefined || repository === "") {
    console.error("GITHUB_TOKEN and GITHUB_REPOSITORY are required");
    process.exit(2);
  }
  const github = new GitHub(process.env.GITHUB_API_URL ?? "https://api.github.com", token, repository);
  console.log(await notify(github, process.env));
}
