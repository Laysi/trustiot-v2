// Decides whether a scheduled nightly deploys at all (Laysi/trustiot-v2#433). Everything the
// nightly deploys is pinned by version and digest, so a night that would test the same tree as the
// last run that passed tells nothing new. The `decide` job of nightly.yml runs this script on
// scheduled runs only and hands one boolean, `deploy`, to the other jobs:
//
// - scheduled run: compare the tree of this commit with the tree of the commit the newest
//   completed scheduled or manual nightly run on main tested. Same tree and that run passed:
//   `deploy=false`. Anything else, or anything that cannot be read: `deploy=true`.
// - any other event (manual runs, pull requests): `deploy=true` without reading anything.
//
// The newest completed run is the one that counts, not the newest successful one: when it failed or
// was cancelled, the next night runs again even though main did not change, so the failure issue
// (notify.ts) is retried and closed by the first night that passes. A manual run with
// `simulate_failure` ends as a failed run, so it counts as one too: the night after it runs and
// closes the issue it opened. Policy and cost: docs/architecture/ci-release.md, nightly.yml.
//
// Usage in the workflow: bun scripts/nightly/unchanged.ts
//   reads GITHUB_EVENT_NAME, GITHUB_SHA, GITHUB_RUN_ID, GITHUB_REPOSITORY, GITHUB_API_URL,
//   GITHUB_TOKEN (actions: read, contents: read); writes `deploy=<true|false>` to GITHUB_OUTPUT and a
//   line to GITHUB_STEP_SUMMARY. It never fails the job: every problem means `deploy=true`.
//
// Read-only look at the real data from a workstation (GET requests only, nothing is written):
//   GITHUB_TOKEN=<token> GITHUB_REPOSITORY=<owner>/<name> bun scripts/nightly/unchanged.ts --dry-run [--event schedule] [--sha <commit>]
//   prints the decision and its evidence: the commit and tree compared, and the newest completed
//   runs it looked at. Without GITHUB_SHA or --sha the commit is the tip of main.
import { appendFileSync } from "node:fs";
import { posix } from "node:path";
import { parseArgs } from "node:util";

export const NIGHTLY_WORKFLOW_PATH = ".github/workflows/nightly.yml";
export const DEFAULT_BRANCH = "main";
/** The events whose runs on main count as "the last nightly". */
export const COUNTED_EVENTS: readonly string[] = ["schedule", "workflow_dispatch"];
/** One page is far more than the newest completed runs this needs. */
const RUNS_PER_PAGE = 100;
const REQUEST_TIMEOUT_MS = 30_000;
const SHOWN_RUNS = 5;

/** A completed run of nightly.yml, reduced to what the decision reads. */
export interface NightlyRun {
  id: number;
  url: string;
  event: string;
  branch: string;
  /** `success`, `failure`, `cancelled`, `timed_out`, and so on. */
  conclusion: string;
  /** The commit the run tested. */
  sha: string;
  /** ISO 8601 time the run was created. */
  createdAt: string;
}

export interface Decision {
  deploy: boolean;
  /** Why, in one line. */
  reason: string;
  /** The tree of the current commit, when it was read. */
  tree?: string;
  /** The run the current tree was compared with, when there was one. */
  compared?: NightlyRun & { tree?: string };
}

/** What the decision needs from GitHub; tests replace it. */
export interface Source {
  /** Completed nightly runs on the default branch, in any order. */
  listRuns(): Promise<NightlyRun[]>;
  /** The tree (the object that is the whole file tree) of a commit. */
  treeOf(sha: string): Promise<string>;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function text(value: unknown, what: string): string {
  if (typeof value !== "string" || value === "") {
    throw new Error(`${what} is missing or not a string`);
  }
  return value;
}

/** Reads the body of GET /repos/{repo}/actions/workflows/nightly.yml/runs. Throws on a shape it does not know. */
export function parseRuns(body: unknown): NightlyRun[] {
  if (!isRecord(body) || !Array.isArray(body.workflow_runs)) {
    throw new Error("the run list has no workflow_runs array");
  }
  return body.workflow_runs.map((raw: unknown, index: number): NightlyRun => {
    if (!isRecord(raw)) {
      throw new Error(`workflow_runs[${index}] is not an object`);
    }
    const id = raw.id;
    if (typeof id !== "number" || !Number.isInteger(id)) {
      throw new Error(`workflow_runs[${index}].id is not an integer`);
    }
    const status = typeof raw.status === "string" ? raw.status : "";
    return {
      id,
      url: text(raw.html_url, `workflow_runs[${index}].html_url`),
      event: text(raw.event, `workflow_runs[${index}].event`),
      branch: typeof raw.head_branch === "string" ? raw.head_branch : "",
      // A run that has not completed has no conclusion: it never matches `success`, and the
      // filter below drops it anyway.
      conclusion: status === "completed" && typeof raw.conclusion === "string" ? raw.conclusion : "",
      sha: text(raw.head_sha, `workflow_runs[${index}].head_sha`),
      createdAt: text(raw.created_at, `workflow_runs[${index}].created_at`),
    };
  });
}

/**
 * The completed scheduled and manual runs on main, newest first, without the current run. A pull
 * request run is no candidate even when its branch is called main (a fork can do that): only the
 * two events count.
 */
export function candidates(runs: readonly NightlyRun[], currentRunId?: number): NightlyRun[] {
  return runs
    .filter(
      (run) =>
        run.conclusion !== "" &&
        run.branch === DEFAULT_BRANCH &&
        COUNTED_EVENTS.includes(run.event) &&
        run.id !== currentRunId,
    )
    .toSorted((a, b) => Date.parse(b.createdAt) - Date.parse(a.createdAt) || b.id - a.id);
}

function describeRun(run: NightlyRun): string {
  return `${run.url} (${run.event}, ${run.createdAt}, ${run.conclusion}, commit ${run.sha.slice(0, 12)})`;
}

/**
 * Whether this run deploys. Never throws: a run that cannot be judged deploys, which is always the
 * safe answer.
 */
export async function decide(
  input: { eventName: string; sha: string; runId?: number },
  source: Source,
): Promise<Decision> {
  if (input.eventName !== "schedule") {
    return { deploy: true, reason: `a ${input.eventName || "unknown"} event always deploys` };
  }
  const message = (error: unknown) => (error instanceof Error ? error.message : String(error));
  let latest: NightlyRun | undefined;
  try {
    latest = candidates(await source.listRuns(), input.runId)[0];
  } catch (error) {
    return { deploy: true, reason: `the nightly runs cannot be read: ${message(error)}` };
  }
  if (latest === undefined) {
    return { deploy: true, reason: "no completed scheduled or manual nightly run on main was found" };
  }
  if (latest.conclusion !== "success") {
    return {
      deploy: true,
      reason: `the newest completed nightly run ${describeRun(latest)} did not pass, so this night runs again`,
      compared: latest,
    };
  }
  let tree: string;
  let comparedTree: string;
  try {
    tree = await source.treeOf(input.sha);
    comparedTree = latest.sha === input.sha ? tree : await source.treeOf(latest.sha);
  } catch (error) {
    return { deploy: true, reason: `the trees cannot be read: ${message(error)}`, compared: latest };
  }
  if (tree === comparedTree) {
    return {
      deploy: false,
      reason: `main's tree ${tree} is the tree of ${latest.sha.slice(0, 12)}, which the passed nightly run ${describeRun(latest)} tested`,
      tree,
      compared: { ...latest, tree: comparedTree },
    };
  }
  return {
    deploy: true,
    reason: `main's tree ${tree} differs from ${comparedTree} (commit ${latest.sha.slice(0, 12)}) of the newest passed nightly run ${describeRun(latest)}`,
    tree,
    compared: { ...latest, tree: comparedTree },
  };
}

/** The line for the job summary. */
export function summaryLine(decision: Decision): string {
  return decision.deploy
    ? `Nightly deploys: ${decision.reason}.\n`
    : `Nightly skipped, nothing was deployed: ${decision.reason}.\n`;
}

const REPOSITORY = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;

/** GET-only access to the GitHub REST API for the two reads the decision needs. */
export class GitHubSource implements Source {
  constructor(
    private readonly options: {
      apiUrl: string;
      repository: string;
      token: string;
      fetchImpl?: typeof fetch;
    },
  ) {
    if (!REPOSITORY.test(options.repository)) {
      throw new Error(`"${options.repository}" is not an owner/name repository`);
    }
    if (options.token === "") {
      throw new Error("GITHUB_TOKEN is not set");
    }
  }

  private async get(path: string): Promise<unknown> {
    const base = this.options.apiUrl.replace(/\/+$/, "");
    const url = `${base}/repos/${this.options.repository}${path}`;
    const response = await (this.options.fetchImpl ?? fetch)(url, {
      method: "GET",
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: `Bearer ${this.options.token}`,
        "X-GitHub-Api-Version": "2022-11-28",
      },
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    if (!response.ok) {
      throw new Error(`GET ${path.split("?")[0]} failed with HTTP ${response.status}`);
    }
    return response.json();
  }

  async listRuns(): Promise<NightlyRun[]> {
    const workflow = posix.basename(NIGHTLY_WORKFLOW_PATH);
    const query = new URLSearchParams({
      branch: DEFAULT_BRANCH,
      status: "completed",
      exclude_pull_requests: "true",
      per_page: String(RUNS_PER_PAGE),
    });
    return parseRuns(await this.get(`/actions/workflows/${workflow}/runs?${query}`));
  }

  async treeOf(sha: string): Promise<string> {
    if (!/^[0-9a-f]{40}$/.test(sha)) {
      throw new Error(`"${sha}" is not a full commit SHA`);
    }
    const body = await this.get(`/git/commits/${sha}`);
    const tree = isRecord(body) && isRecord(body.tree) ? body.tree.sha : undefined;
    return text(tree, `tree.sha of commit ${sha}`);
  }

  /** The commit at the tip of main, for a dry run outside Actions. */
  async mainTip(): Promise<string> {
    const body = await this.get(`/git/ref/heads/${DEFAULT_BRANCH}`);
    const object = isRecord(body) ? body.object : undefined;
    return text(isRecord(object) ? object.sha : undefined, `object.sha of ${DEFAULT_BRANCH}`);
  }
}

/** The evidence a dry run prints: the decision and the newest completed runs it looked at. */
export function evidence(decision: Decision, runs: readonly NightlyRun[], eventName: string, sha: string): string {
  const lines = [
    `decision: deploy=${decision.deploy}`,
    `reason: ${decision.reason}`,
    `evaluated as: ${eventName} event, commit ${sha}`,
  ];
  if (decision.tree !== undefined) lines.push(`tree of the commit: ${decision.tree}`);
  if (decision.compared !== undefined) {
    lines.push(`compared run: ${describeRun(decision.compared)}`);
    if (decision.compared.tree !== undefined) lines.push(`tree of the compared run's commit: ${decision.compared.tree}`);
  }
  const newest = candidates(runs).slice(0, SHOWN_RUNS);
  lines.push(`newest completed scheduled or manual runs on ${DEFAULT_BRANCH} (${newest.length} of ${runs.length} listed):`);
  for (const run of newest) lines.push(`  - ${describeRun(run)}`);
  return `${lines.join("\n")}\n`;
}

/** The workflow's entry: never throws, never fails the job. */
export async function runInWorkflow(env: Record<string, string | undefined>, source?: Source): Promise<Decision> {
  const eventName = env.GITHUB_EVENT_NAME ?? "";
  let decision: Decision;
  try {
    const runId = Number.parseInt(env.GITHUB_RUN_ID ?? "", 10);
    // Built on first use, so that a manual run, which reads nothing, needs no token.
    const github = () =>
      new GitHubSource({
        apiUrl: env.GITHUB_API_URL || "https://api.github.com",
        repository: env.GITHUB_REPOSITORY ?? "",
        token: env.GITHUB_TOKEN ?? "",
      });
    const src: Source = source ?? {
      listRuns: () => github().listRuns(),
      treeOf: (commit) => github().treeOf(commit),
    };
    decision = await decide({ eventName, sha: env.GITHUB_SHA ?? "", ...(Number.isNaN(runId) ? {} : { runId }) }, src);
  } catch (error) {
    decision = { deploy: true, reason: `the decision failed: ${error instanceof Error ? error.message : String(error)}` };
  }
  console.log(`deploy=${decision.deploy}: ${decision.reason}`);
  if (env.GITHUB_OUTPUT) appendFileSync(env.GITHUB_OUTPUT, `deploy=${decision.deploy}\n`);
  if (env.GITHUB_STEP_SUMMARY) appendFileSync(env.GITHUB_STEP_SUMMARY, summaryLine(decision));
  return decision;
}

async function dryRun(env: Record<string, string | undefined>, args: { event?: string; sha?: string }): Promise<number> {
  const token = env.GITHUB_TOKEN ?? "";
  const repository = env.GITHUB_REPOSITORY ?? "";
  let source: GitHubSource;
  try {
    source = new GitHubSource({ apiUrl: env.GITHUB_API_URL || "https://api.github.com", repository, token });
  } catch (error) {
    console.error(`${error instanceof Error ? error.message : String(error)} (set GITHUB_TOKEN and GITHUB_REPOSITORY)`);
    return 2;
  }
  const eventName = args.event ?? (env.GITHUB_EVENT_NAME || "schedule");
  let sha = args.sha ?? env.GITHUB_SHA;
  if (!sha) {
    try {
      sha = await source.mainTip();
    } catch (error) {
      console.error(`the tip of ${DEFAULT_BRANCH} cannot be read: ${error instanceof Error ? error.message : String(error)}`);
      return 1;
    }
  }
  // List once so that the evidence shows exactly what the decision saw.
  let runs: NightlyRun[];
  try {
    runs = await source.listRuns();
  } catch (error) {
    console.error(`the nightly runs cannot be read: ${error instanceof Error ? error.message : String(error)}`);
    return 1;
  }
  const decision = await decide({ eventName, sha }, { listRuns: async () => runs, treeOf: (commit) => source.treeOf(commit) });
  process.stdout.write(evidence(decision, runs, eventName, sha));
  return 0;
}

if (import.meta.main) {
  const { values } = parseArgs({
    options: { "dry-run": { type: "boolean" }, event: { type: "string" }, sha: { type: "string" } },
    strict: true,
  });
  if (values["dry-run"]) {
    process.exit(await dryRun(process.env, { event: values.event, sha: values.sha }));
  }
  await runInWorkflow(process.env);
}
