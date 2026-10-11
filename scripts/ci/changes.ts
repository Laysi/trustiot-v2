/**
 * Decides which path-dependent jobs of ci.yml run (docs/architecture/ci-release.md, section
 * "依變更路徑決定 job"). The `changes` job runs this script and passes one output per job,
 * `true` or `false`; each job runs unless its output is exactly `false`, so a failure of this
 * script, or of the `changes` job, runs everything.
 *
 * What is compared:
 * - pull_request: the merge commit GitHub checks out against its first parent, the tip of the
 *   base branch it was merged onto, so the files are exactly what merging would change.
 * - push (main): the pushed commit against the newest earlier main commit (first-parent
 *   ancestor) whose newest ci run succeeded. Commits whose ci is still running, failed, was
 *   cancelled or never ran are passed over, so their changes are compared too. A skipped job
 *   therefore always means "the files it reads are unchanged since a commit where all of ci
 *   passed", and a red main stays red until it is fixed (decision 30). When no green commit is
 *   found among the last MAIN_HISTORY_LIMIT, or the runs cannot be read, every job runs.
 * - any other event or a git failure: every job runs.
 *
 * Path rules: `rust` is the safe direction. It runs unless every changed file is in a short
 * list of paths that nothing in the rust job reads; a new top-level directory or file runs it
 * by default. `cargo-deny`, `helm` and `host` read a known, small set of files and run when one of
 * them changes. Changes to this script or any repository file it imports (transitively;
 * DECISION_CODE), ci.yml, the local actions or the tool pins run every job. `gitleaks` and `workflow-policy` do not depend on this script and always run.
 *
 * Usage (in the `changes` job): bun scripts/ci/changes.ts
 * Reads GITHUB_EVENT_NAME, GITHUB_REPOSITORY, GITHUB_API_URL, GITHUB_TOKEN (actions: read, for
 * push events) and the git history (fetch-depth 0); writes GITHUB_OUTPUT and GITHUB_STEP_SUMMARY.
 * Exit status: always 0 once the outputs are written; problems turn into "run everything".
 */

import { appendFileSync } from "node:fs";
import { posix, resolve } from "node:path";

import { CI_WORKFLOW_PATH, newestMainCiRun, parseWorkflowRuns, type WorkflowRun } from "../release/lib.ts";

/** The ci.yml jobs whose run depends on the changed paths, by job id. */
export const PATH_JOBS = ["rust", "cargo-deny", "helm", "host"] as const;
export type PathJob = (typeof PATH_JOBS)[number];

export interface JobDecision {
  run: boolean;
  /** Why, in one line: the first file that runs the job, or why nothing does. */
  reason: string;
}
export type Decision = Record<PathJob, JobDecision>;

/**
 * This script and every repository file it imports, directly or through another import: a
 * change to any of them can change the decision, so it runs every job and the new code is
 * tested. `../release/lib.ts` picks the green main commit a push is compared with.
 * changes.test.ts follows the imports and fails when one is missing here.
 */
export const DECISION_CODE: ReadonlySet<string> = new Set(["scripts/ci/changes.ts", "scripts/release/lib.ts"]);

/**
 * Files every path-dependent job depends on: the workflow, its local actions, this script and
 * its imports, and the tool pins (`rust`, `cargo-deny`, `bun`, `helm` and `kubeconform` come
 * from them).
 */
export function affectsEveryJob(path: string): boolean {
  return (
    path === ".github/workflows/ci.yml" ||
    path.startsWith(".github/actions/") ||
    DECISION_CODE.has(path) ||
    path === ".prototools" ||
    path.startsWith(".proto/")
  );
}

/**
 * Paths nothing in the rust job reads (checked by changes.test.ts against every path literal in
 * the Rust sources). Everything else runs it, including paths this list has never heard of.
 */
const RUST_IGNORED_PREFIXES = [
  ".githooks/",
  // Workflows other than ci.yml (ci.yml and the local actions run every job, see above).
  ".github/",
  "deploy/",
  "docs/",
  "frontend/",
  "scripts/",
] as const;
const RUST_IGNORED_FILES: ReadonlySet<string> = new Set([
  ".dockerignore",
  ".gitleaksignore",
  "README.md",
  "deny.toml",
  "renovate.json5",
]);
/** Files under an ignored prefix that a Rust test reads. */
const RUST_READ_FILES: ReadonlySet<string> = new Set([
  // crates/service-kit/tests/chart_contract.rs
  "deploy/chart/values.yaml",
]);

/**
 * Files the rust job reads wherever they are: `tools/repo-check` fails on any file named
 * AGENTS.md (any letter case); `tools/sql-check` scans every `.rs` file of the repository and
 * the SQL files they include, so SQL counts too.
 */
function isReadAnywhere(path: string): boolean {
  const name = posix.basename(path).toLowerCase();
  return name === "agents.md" || name.endsWith(".rs") || name.endsWith(".sql");
}

export function affectsRust(path: string): boolean {
  if (affectsEveryJob(path) || isReadAnywhere(path) || RUST_READ_FILES.has(path)) {
    return true;
  }
  if (RUST_IGNORED_FILES.has(path)) {
    return false;
  }
  return !RUST_IGNORED_PREFIXES.some((prefix) => path.startsWith(prefix));
}

/**
 * cargo-deny reads the dependency graph (every Cargo.toml, Cargo.lock, Cargo configuration),
 * its rules (deny.toml), and the toolchain pin `cargo metadata` runs with.
 */
export function affectsCargoDeny(path: string): boolean {
  const name = posix.basename(path);
  return (
    affectsEveryJob(path) ||
    name === "Cargo.toml" ||
    name === "Cargo.lock" ||
    path === "deny.toml" ||
    path === "rust-toolchain.toml" ||
    path.startsWith(".cargo/") ||
    path.includes("/.cargo/")
  );
}

/** Files outside the chart that `bun test deploy/chart` reads (checked by changes.test.ts). */
const HELM_READ_FILES: ReadonlySet<string> = new Set([
  "docs/architecture/deployment.md",
  "scripts/lib/stack.ts",
  "services/ingest/tests/common/mod.rs",
]);

/**
 * What `bun test deploy/flux` reads besides deploy/flux/ and the chart (Laysi/trustiot-v2#70):
 * the SOPS rules and the scripts that its tests load (checked by changes.test.ts).
 */
const FLUX_READ_FILES: ReadonlySet<string> = new Set([".sops.yaml", "scripts/release/flux.ts"]);

/**
 * `bun test deploy/chart` and `bun test deploy/flux`: the chart, its tests, the mesh values, the
 * SaaS GitOps tree and a few cross-checked files.
 */
export function affectsHelm(path: string): boolean {
  return (
    affectsEveryJob(path) ||
    path.startsWith("deploy/chart/") ||
    path.startsWith("deploy/istio/") ||
    path.startsWith("deploy/flux/") ||
    path.startsWith("scripts/flux/") ||
    HELM_READ_FILES.has(path) ||
    FLUX_READ_FILES.has(path)
  );
}

/**
 * The SaaS host installer: its sources, tests, lock file and build all live in `deploy/host`
 * (checked by changes.test.ts).
 */
export function affectsHost(path: string): boolean {
  return affectsEveryJob(path) || path.startsWith("deploy/host/");
}

const RULES: Record<PathJob, (path: string) => boolean> = {
  rust: affectsRust,
  "cargo-deny": affectsCargoDeny,
  helm: affectsHelm,
  host: affectsHost,
};

/** Every job runs, for the given reason. */
export function runEverything(reason: string): Decision {
  return Object.fromEntries(PATH_JOBS.map((job) => [job, { run: true, reason }])) as Decision;
}

/** Which jobs the changed files (repository-relative, `/`-separated) run. */
export function decide(files: readonly string[]): Decision {
  return Object.fromEntries(
    PATH_JOBS.map((job): [PathJob, JobDecision] => {
      const trigger = files.find(RULES[job]);
      if (trigger !== undefined) {
        return [job, { run: true, reason: `\`${trigger}\` changed` }];
      }
      return [
        job,
        {
          run: false,
          reason: files.length === 0 ? "no file changed" : `none of the ${files.length} changed file(s) affects it`,
        },
      ];
    }),
  ) as Decision;
}

// ---------------------------------------------------------------------------------------------
// The commits to compare

const COMMIT_SHA = /^[0-9a-f]{40}$/;
const REPOSITORY = /^[A-Za-z0-9-]+\/[A-Za-z0-9._-]+$/;

/** How many main commits back the search for the newest green one goes. */
export const MAIN_HISTORY_LIMIT = 50;

function git(root: string, args: readonly string[]): string {
  const result = Bun.spawnSync(["git", ...args], { cwd: root, stdout: "pipe", stderr: "pipe" });
  if (result.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr.toString().trim()}`);
  }
  return result.stdout.toString();
}

/**
 * The files that differ between two commits. `--no-renames` lists both sides of a rename, so a
 * file moved out of a Rust directory still counts there; `-z` keeps unusual names unquoted.
 */
export function changedFiles(root: string, base: string, head: string): string[] {
  return git(root, ["diff", "--name-only", "--no-renames", "-z", base, head, "--"])
    .split("\0")
    .filter((path) => path !== "");
}

/** The two commits a pull request run compares, or why every job must run instead. */
export type Range = { base: string; head: string } | { everything: string };

/**
 * A pull request run checks out the merge commit of the pull request into its base branch:
 * compare it with its first parent, the tip of the base branch it was merged onto.
 */
export function pullRequestRange(root: string): Range {
  const [head = "", ...parents] = git(root, ["rev-list", "--parents", "-n", "1", "HEAD"]).trim().split(" ");
  if (parents.length !== 2 || !COMMIT_SHA.test(parents[0] ?? "")) {
    return { everything: "the checked-out commit is not the pull request's merge commit" };
  }
  return { base: parents[0] as string, head };
}

/** The commits main pointed to before HEAD, newest first: HEAD's first-parent ancestors. */
export function mainAncestors(root: string, limit = MAIN_HISTORY_LIMIT): string[] {
  const parents = git(root, ["rev-list", "--parents", "-n", "1", "HEAD"]).trim().split(" ").slice(1);
  if (parents.length === 0) {
    return [];
  }
  return git(root, ["rev-list", "--first-parent", `--max-count=${limit}`, parents[0] as string, "--"])
    .split("\n")
    .filter((commit) => commit !== "");
}

/** Lists the recent push runs of ci.yml on main; injected so that tests need no network. */
export type ListRuns = () => Promise<WorkflowRun[]>;

/** One main commit the search passed over, and why. */
function notGreen(commit: string, runs: readonly WorkflowRun[]): string {
  const newest = newestMainCiRun(runs, commit);
  const state =
    newest === undefined
      ? "no ci run"
      : newest.status !== "completed"
        ? newest.status || "not finished"
        : newest.conclusion || "no conclusion";
  return `${commit.slice(0, 12)} (${state})`;
}

/**
 * The newest of `ancestors` (newest first) whose newest push run of ci.yml on main succeeded,
 * with the newer ones passed over; or why none qualifies.
 */
export function newestGreen(
  ancestors: readonly string[],
  runs: readonly WorkflowRun[],
): { commit: string; passedOver: string[] } | { problem: string } {
  const passedOver: string[] = [];
  for (const commit of ancestors) {
    const newest = newestMainCiRun(runs, commit);
    if (newest?.status === "completed" && newest.conclusion === "success") {
      return { commit, passedOver };
    }
    passedOver.push(notGreen(commit, runs));
  }
  return {
    problem:
      `none of the last ${ancestors.length} main commit(s) has a successful ci run` +
      (passedOver.length > 0 ? ` (${passedOver.slice(0, 5).join(", ")}${passedOver.length > 5 ? ", ..." : ""})` : ""),
  };
}

/**
 * The GitHub REST request (list workflow runs, needs `actions: read`) for the most recent push
 * runs of ci.yml on main: one page of 100, more than MAIN_HISTORY_LIMIT commits ever need.
 */
export function mainRunsUrl(apiUrl: string, repository: string): string {
  if (!REPOSITORY.test(repository)) {
    throw new Error(`"${repository}" is not an owner/name repository`);
  }
  const workflow = posix.basename(CI_WORKFLOW_PATH);
  const query = new URLSearchParams({ branch: "main", event: "push", per_page: "100" });
  return `${apiUrl.replace(/\/+$/, "")}/repos/${repository}/actions/workflows/${workflow}/runs?${query}`;
}

/** The GitHub API call behind `ListRuns`. */
export function githubListRuns(env: Record<string, string | undefined>): ListRuns {
  return async () => {
    const token = env.GITHUB_TOKEN;
    if (!token) {
      throw new Error("GITHUB_TOKEN is not set");
    }
    const response = await fetch(mainRunsUrl(env.GITHUB_API_URL || "https://api.github.com", env.GITHUB_REPOSITORY ?? ""), {
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: `Bearer ${token}`,
        "X-GitHub-Api-Version": "2022-11-28",
      },
      signal: AbortSignal.timeout(30_000),
    });
    if (!response.ok) {
      throw new Error(`listing the ci runs on main failed with HTTP ${response.status}`);
    }
    return parseWorkflowRuns(await response.json());
  };
}

/** The decision for one workflow run, with what it was made from. */
export interface Plan {
  decision: Decision;
  /** The changed files; empty when nothing was compared. */
  files: string[];
  /** The commit the files were compared with, and how it was chosen. */
  base?: string;
  /** Set when every job runs without comparing paths. */
  everything?: string;
}

/** Decides for the current workflow run; never throws, every problem runs everything. */
export async function plan(root: string, env: Record<string, string | undefined>, listRuns: ListRuns): Promise<Plan> {
  const everything = (reason: string): Plan => ({ decision: runEverything(reason), files: [], everything: reason });
  const message = (error: unknown) => (error instanceof Error ? error.message : String(error));
  try {
    const eventName = env.GITHUB_EVENT_NAME ?? "";
    const head = git(root, ["rev-parse", "HEAD"]).trim();
    let base: string;
    let how: string;
    if (eventName === "pull_request") {
      const range = pullRequestRange(root);
      if ("everything" in range) {
        return everything(range.everything);
      }
      base = range.base;
      how = "the tip of the base branch (first parent of the merge commit)";
    } else if (eventName === "push") {
      const ancestors = mainAncestors(root);
      if (ancestors.length === 0) {
        return everything("the pushed commit has no parent");
      }
      let runs: WorkflowRun[];
      try {
        runs = await listRuns();
      } catch (error) {
        return everything(`the ci runs on main cannot be read: ${message(error)}`);
      }
      const green = newestGreen(ancestors, runs);
      if ("problem" in green) {
        return everything(green.problem);
      }
      base = green.commit;
      how =
        "the newest main commit whose ci passed" +
        (green.passedOver.length > 0 ? `; passed over ${green.passedOver.join(", ")}` : "");
    } else {
      return everything(`the \`${eventName || "unknown"}\` event is not compared by path`);
    }
    const files = changedFiles(root, base, head);
    return { decision: decide(files), files, base: `${base} (${how})` };
  } catch (error) {
    return everything(`the changed files cannot be determined: ${message(error)}`);
  }
}

/** `key=value` lines for GITHUB_OUTPUT. */
export function outputLines(decision: Decision): string {
  return PATH_JOBS.map((job) => `${job}=${decision[job].run}\n`).join("");
}

const SUMMARY_FILE_LIMIT = 200;

/** The job summary: what runs and why, then the changed files. */
export function summary(result: Plan): string {
  const lines = ["## Path-dependent jobs", "", "| job | runs | why |", "| --- | --- | --- |"];
  for (const job of PATH_JOBS) {
    const { run, reason } = result.decision[job];
    lines.push(`| \`${job}\` | ${run ? "yes" : "skipped"} | ${reason.replaceAll("|", "\\|")} |`);
  }
  lines.push("", "`gitleaks` and `workflow-policy` always run.", "");
  if (result.everything === undefined) {
    lines.push(`Compared with ${result.base ?? "?"}.`, "", `### Changed files (${result.files.length})`, "");
    for (const file of result.files.slice(0, SUMMARY_FILE_LIMIT)) {
      lines.push(`- \`${file}\``);
    }
    if (result.files.length > SUMMARY_FILE_LIMIT) {
      lines.push(`- ... and ${result.files.length - SUMMARY_FILE_LIMIT} more`);
    }
  }
  return `${lines.join("\n")}\n`;
}

export async function main(env: Record<string, string | undefined>): Promise<number> {
  const root = resolve(import.meta.dir, "..", "..");
  const result = await plan(root, env, githubListRuns(env));
  if (result.everything !== undefined) {
    // A notice, not a warning: every job running is always a safe answer.
    console.log(`::notice title=changes::Every job runs: ${result.everything.replaceAll("\n", " ")}`);
  }
  if (result.base !== undefined) {
    console.log(`compared with ${result.base}: ${result.files.length} changed file(s)`);
  }
  for (const job of PATH_JOBS) {
    console.log(`${job}: ${result.decision[job].run ? "runs" : "skipped"} (${result.decision[job].reason})`);
  }
  if (env.GITHUB_OUTPUT) {
    appendFileSync(env.GITHUB_OUTPUT, outputLines(result.decision));
  }
  if (env.GITHUB_STEP_SUMMARY) {
    appendFileSync(env.GITHUB_STEP_SUMMARY, summary(result));
  }
  return 0;
}

if (import.meta.main) {
  process.exit(await main(process.env));
}
