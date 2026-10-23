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
 *   Before that, a push also asks whether its tree is exactly the tree that the merged pull
 *   request's ci runs tested (tested-tree.ts, Laysi/trustiot-v2#431): a job that ran and passed in
 *   every one of those runs is skipped; every other job follows the path rules above. Only
 *   GitHub API metadata and git decide this, never anything a pull request run produced.
 * - any other event or a git failure: every job runs.
 *
 * Path rules: `rust` is the safe direction. It runs unless every changed file is in a short
 * list of paths that nothing in the rust job reads; a new top-level directory or file runs it
 * by default. `cargo-deny`, `helm`, `host`, `postgres-image` and `frontend` read a known, small set of files
 * and run when one of them changes. Changes to this script or any repository file it imports (transitively;
 * DECISION_CODE), ci.yml, the local actions or the tool pins run every job. `gitleaks` and `workflow-policy` do not depend on this script and always run.
 *
 * Usage (in the `changes` job): bun scripts/ci/changes.ts
 * Reads GITHUB_EVENT_NAME, GITHUB_REPOSITORY, GITHUB_API_URL, GITHUB_TOKEN (for push events:
 * actions, contents, issues and pull-requests read) and the git history (fetch-depth 0); writes
 * GITHUB_OUTPUT and GITHUB_STEP_SUMMARY.
 * Exit status: always 0 once the outputs are written; problems turn into "run everything".
 *
 * Read-only explanation of the decision for a main commit, with real API data (writes nothing):
 *   GITHUB_TOKEN=<token with read access> bun scripts/ci/changes.ts --explain <commit on main>
 * The commit and its history must be in the local clone (`git fetch origin main` first);
 * GITHUB_REPOSITORY defaults to Laysi/trustiot-v2. Exit status 2 on a usage error.
 */

import { appendFileSync } from "node:fs";
import { posix, resolve } from "node:path";

import { CI_WORKFLOW_PATH, newestMainCiRun, type WorkflowRun } from "../release/lib.ts";
import {
  type GitHubReads,
  MAIN_BRANCH,
  parseCommitTree,
  parseForcePushTimes,
  parseGitHubCreated,
  parseIssueEventTypes,
  parseJobs,
  parseMergeBase,
  parsePullNumbers,
  parsePullRequests,
  parseRuns,
  type Run,
  type TestedTree,
  testedTree,
} from "./tested-tree.ts";

/** The ci.yml jobs whose run depends on the changed paths, by job id. */
export const PATH_JOBS = ["rust", "cargo-deny", "helm", "host", "postgres-image", "frontend"] as const;
export type PathJob = (typeof PATH_JOBS)[number];

/**
 * Jobs whose result on the same tree changes with time, so a pass in the pull request says
 * nothing about main later: cargo-deny reads the RustSec advisory database when it runs (on
 * 2026-10-14, PR Laysi/trustiot-v2#411 passed it and main failed it hours later on new rustls
 * advisories). They never take the same-tree skip; the path rules decide them on main.
 */
export const TIME_DEPENDENT_JOBS: ReadonlySet<PathJob> = new Set(["cargo-deny"]);

export interface JobDecision {
  run: boolean;
  /** Why, in one line: the first file that runs the job, or why nothing does. */
  reason: string;
}
export type Decision = Record<PathJob, JobDecision>;

/**
 * This script and every repository file it imports, directly or through another import: a
 * change to any of them can change the decision, so it runs every job and the new code is
 * tested. `../release/lib.ts` picks the green main commit a push is compared with, and
 * `./tested-tree.ts` decides which jobs already passed on the pushed tree.
 * changes.test.ts follows the imports and fails when one is missing here.
 */
export const DECISION_CODE: ReadonlySet<string> = new Set([
  "scripts/ci/changes.ts",
  "scripts/ci/tested-tree.ts",
  "scripts/release/lib.ts",
]);

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
  // tools/migrate/tests/registry.rs: which login roles the chart binds a Secret to
  "deploy/chart/templates/_migrate.tpl",
  // crates/testkit/src/chart.rs: the Postgres connection budgets that the services' tests
  // (services/*/tests/chart_connections.rs) hold their pools to
  "deploy/chart/templates/_postgres.tpl",
  // tools/migrate/tests/mqtt_broker.rs and services/certs/tests/broker_certificate.rs run the MQTT
  // broker's configuration and its gateway listener (include_str!)
  "deploy/chart/files/mosquitto/mosquitto.conf",
  "deploy/chart/files/mosquitto/gateway-listener.conf",
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
  // The MQTT broker's image pin, and the probe and memory limit its configuration test runs.
  "crates/protocol/tests/broker.rs",
  // The bundled Postgres e2e runs it as the database owner.
  "crates/service-kit/sql/database.sql",
  // The collector's key redaction rules are checked against service-kit's names.
  "crates/service-kit/src/telemetry/redact.rs",
  // The bundled Postgres must match the image's PostgreSQL major and AGE version.
  "deploy/images/postgres/Containerfile",
  "docs/architecture/deployment.md",
  // Rule 5 applied to the chart rendered with every optional component on (Laysi/trustiot-v2#398).
  "scripts/ci/check-secret-handling.ts",
  "scripts/lib/stack.ts",
  "services/ingest/tests/common/mod.rs",
  // The labels and annotations the Job keeps on the password Secrets it binds, and the broker
  // certificate CronJob's own annotation among them.
  "tools/migrate/src/mqtt_certificate.rs",
  "tools/migrate/src/passwords.rs",
  "tools/migrate/tests/mqtt_broker.rs",
]);

/**
 * What `bun test deploy/flux` reads besides deploy/flux/ and the chart (Laysi/trustiot-v2#70):
 * the SOPS rules and the scripts that its tests load (checked by changes.test.ts), such as the
 * Service range's static band for the demo overlay's fixed ingress ClusterIP.
 */
const FLUX_READ_FILES: ReadonlySet<string> = new Set([".sops.yaml", "scripts/release/flux.ts", "scripts/nightly/ingress-address.ts"]);

/**
 * ClickHouse migrations: the chart tests check the collector's exporter columns against the log
 * table migration, and the log pipeline e2e test applies the whole directory (it names the
 * directory itself, so the rule accepts the directory path and every file under it).
 */
const HELM_READ_DIR = "services/ingest/migrations/clickhouse";

/**
 * `bun test deploy/chart` and `bun test deploy/flux`: the chart, its tests, the mesh values, the
 * ClickHouse migrations, the SaaS GitOps tree and a few cross-checked files.
 */
export function affectsHelm(path: string): boolean {
  return (
    affectsEveryJob(path) ||
    path.startsWith("deploy/chart/") ||
    path.startsWith("deploy/istio/") ||
    path.startsWith("deploy/flux/") ||
    path.startsWith("scripts/flux/") ||
    path === HELM_READ_DIR ||
    path.startsWith(`${HELM_READ_DIR}/`) ||
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

/**
 * Files outside `deploy/images/postgres` that its test imports, directly or through another
 * import: the build goes through containerBuildArgs, which checks the context against the
 * secret rules (checked by changes.test.ts). `scripts/release/lib.ts` runs every job anyway.
 */
const POSTGRES_IMAGE_READ_FILES: ReadonlySet<string> = new Set([
  "scripts/ci/check-secret-handling.ts",
  "scripts/lib/build-context.ts",
  "scripts/lib/cleanup.ts",
  "scripts/lib/secrets.ts",
  "scripts/lib/stack.ts",
  "scripts/lib/state.ts",
]);

/**
 * The Postgres image with Apache AGE: its Containerfile (the build context) and its test live in
 * `deploy/images/postgres`.
 */
export function affectsPostgresImage(path: string): boolean {
  return affectsEveryJob(path) || path.startsWith("deploy/images/postgres/") || POSTGRES_IMAGE_READ_FILES.has(path);
}

/**
 * The Rust sources whose message keys reach the browser: frontend/src/i18n/server-keys.test.ts
 * scans `crates/contract/src` and every `services/<name>/src` and fails on a key without a
 * translation, so a Rust change that adds one runs the frontend job (checked by changes.test.ts).
 */
const FRONTEND_MESSAGE_KEY_SOURCES = /^(?:crates\/contract|services\/[^/]+)\/src\/.+\.rs$/;

/**
 * The single-page app: its sources, tests, lock file and build live in `frontend`; the API client
 * is generated from api/openapi.json, and the translations are checked against the message keys
 * the services send.
 */
export function affectsFrontend(path: string): boolean {
  return (
    affectsEveryJob(path) || path.startsWith("frontend/") || path === "api/openapi.json" || FRONTEND_MESSAGE_KEY_SOURCES.test(path)
  );
}

const RULES: Record<PathJob, (path: string) => boolean> = {
  rust: affectsRust,
  "cargo-deny": affectsCargoDeny,
  helm: affectsHelm,
  host: affectsHost,
  "postgres-image": affectsPostgresImage,
  frontend: affectsFrontend,
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

/** The commits main pointed to before `head`, newest first: its first-parent ancestors. */
export function mainAncestors(root: string, limit = MAIN_HISTORY_LIMIT, head = "HEAD"): string[] {
  const parents = git(root, ["rev-list", "--parents", "-n", "1", head]).trim().split(" ").slice(1);
  if (parents.length === 0) {
    return [];
  }
  return git(root, ["rev-list", "--first-parent", `--max-count=${limit}`, parents[0] as string, "--"])
    .split("\n")
    .filter((commit) => commit !== "");
}

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

/** The repository the API calls name; the explanation defaults to this one. */
export const DEFAULT_REPOSITORY = "Laysi/trustiot-v2";

/** One page of results; every list the decision reads must fit in it, or the answer is unknown. */
const PAGE = 100;
/** How many pages of a pull request's events are read before the answer counts as unknown. */
export const ISSUE_EVENT_PAGES = 10;

/**
 * The GitHub REST calls behind the decision, with GITHUB_TOKEN. Every call reads; every failure
 * throws, which the callers turn into running the jobs.
 */
export function githubReads(env: Record<string, string | undefined>): GitHubReads {
  const apiUrl = (env.GITHUB_API_URL || "https://api.github.com").replace(/\/+$/, "");
  const repository = env.GITHUB_REPOSITORY ?? "";
  const workflow = posix.basename(CI_WORKFLOW_PATH);
  const sha = (commit: string) => {
    if (!COMMIT_SHA.test(commit)) {
      throw new Error(`"${commit}" is not a full commit SHA`);
    }
    return commit;
  };

  /** GET `url`; `what` names the request in the error. */
  async function request(url: string, what: string): Promise<unknown> {
    const token = env.GITHUB_TOKEN;
    if (!token) {
      throw new Error("GITHUB_TOKEN is not set");
    }
    const response = await fetch(url, {
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: `Bearer ${token}`,
        "X-GitHub-Api-Version": "2022-11-28",
      },
      signal: AbortSignal.timeout(30_000),
    });
    if (!response.ok) {
      throw new Error(`${what} failed with HTTP ${response.status}`);
    }
    return response.json();
  }

  async function get(path: string, query: Record<string, string> = {}): Promise<unknown> {
    if (!REPOSITORY.test(repository)) {
      throw new Error(`"${repository}" is not an owner/name repository`);
    }
    const search = new URLSearchParams(query).toString();
    return request(`${apiUrl}/repos/${repository}/${path}${search === "" ? "" : `?${search}`}`, `GET ${path}`);
  }

  const commitOf = async (commit: string) => get(`git/commits/${sha(commit)}`);

  return {
    mainRuns: async () => parseRuns(await request(mainRunsUrl(apiUrl, repository), "listing the ci runs on main"), false),
    pullsOfCommit: async (commit) => parsePullRequests(await get(`commits/${sha(commit)}/pulls`, { per_page: String(PAGE) })),
    async pullsWithHead(owner, ref) {
      const numbers = parsePullNumbers(await get("pulls", { state: "all", head: `${owner}:${ref}`, per_page: String(PAGE) }));
      if (numbers.length >= PAGE) {
        throw new Error(`the pull requests with the head ${owner}:${ref} do not fit in one page`);
      }
      return numbers;
    },
    async issueEventTypes(number) {
      const types: string[] = [];
      for (let page = 1; page <= ISSUE_EVENT_PAGES; page++) {
        const events = parseIssueEventTypes(await get(`issues/${number}/events`, { per_page: String(PAGE), page: String(page) }));
        types.push(...events);
        if (events.length < PAGE) {
          return types;
        }
      }
      throw new Error(`pull request #${number} has more than ${ISSUE_EVENT_PAGES * PAGE} events`);
    },
    mergeBase: async (base, head) => parseMergeBase(await get(`compare/${sha(base)}...${sha(head)}`, { per_page: "1" })),
    treeOf: async (commit) => parseCommitTree(await commitOf(commit), commit),
    createdByGitHub: async (commit) => parseGitHubCreated(await commitOf(commit), commit),
    pullRequestRuns: async (head) =>
      parseRuns(await get(`actions/workflows/${workflow}/runs`, { event: "pull_request", head_sha: sha(head), per_page: String(PAGE) }), true),
    // Every attempt: a job re-run until it passed must not count as passed.
    jobsOf: async (runId) => parseJobs(await get(`actions/runs/${runId}/jobs`, { filter: "all", per_page: String(PAGE) })),
    // Newest first, so one page holds the newest force push if there is any.
    forcePushesToMain: async () =>
      parseForcePushTimes(
        await get("activity", { ref: `refs/heads/${MAIN_BRANCH}`, activity_type: "force_push", direction: "desc", per_page: String(PAGE) }),
      ),
  };
}

/** The decision for one workflow run, with what it was made from. */
export interface Plan {
  decision: Decision;
  /** The changed files; empty when nothing was compared. */
  files: string[];
  /** The commit the files were compared with, and how it was chosen. */
  base?: string;
  /** Set when the path rules run every job without comparing paths. */
  everything?: string;
  /** For a push: which jobs passed on the same tree in the merged pull request's runs. */
  tested?: TestedTree<PathJob>;
}

/** The decision for a push of `head` to main; `head` must be checked out or fetched. */
async function planPush(root: string, head: string, repository: string, github: GitHubReads): Promise<Plan> {
  const commit = git(root, ["rev-parse", "--verify", `${head}^{commit}`]).trim();
  const ancestors = mainAncestors(root, MAIN_HISTORY_LIMIT, commit);
  if (ancestors.length === 0) {
    return { decision: runEverything("the pushed commit has no parent"), files: [], everything: "the pushed commit has no parent" };
  }
  let runs: Run[];
  try {
    runs = await github.mainRuns();
  } catch (error) {
    const reason = `the ci runs on main cannot be read: ${error instanceof Error ? error.message : String(error)}`;
    return { decision: runEverything(reason), files: [], everything: reason };
  }
  const tested = await testedTree(root, commit, repository, PATH_JOBS, runs, github);

  // The path rules, compared with the newest green main commit.
  let byPath: Plan;
  const green = newestGreen(ancestors, runs);
  if ("problem" in green) {
    byPath = { decision: runEverything(green.problem), files: [], everything: green.problem };
  } else {
    const how =
      "the newest main commit whose ci passed" + (green.passedOver.length > 0 ? `; passed over ${green.passedOver.join(", ")}` : "");
    const files = changedFiles(root, green.commit, commit);
    byPath = { decision: decide(files), files, base: `${green.commit} (${how})` };
  }

  if (!tested.applies) {
    return { ...byPath, tested };
  }
  const passedThere = `passed in run ${tested.runs.map((run) => run.id).join(", ")} of pull request #${tested.pullRequest} on the same tree`;
  // Only main saves the Rust build cache, and only when its key changed: keep `rust` for that.
  const cacheKey = changedFiles(root, tested.parent, commit).find(affectsRustCacheKey);
  const decision = Object.fromEntries(
    PATH_JOBS.map((job): [PathJob, JobDecision] => {
      if (!tested.passed.includes(job)) {
        return [job, byPath.decision[job]];
      }
      if (TIME_DEPENDENT_JOBS.has(job)) {
        const { run, reason } = byPath.decision[job];
        return [job, { run, reason: `${reason} (it ${passedThere}, but its result changes with time, so the path rules decide)` }];
      }
      if (job === "rust" && cacheKey !== undefined) {
        return [job, { run: true, reason: `${passedThere}, but \`${cacheKey}\` changes the Rust build cache key, so main runs it to save the cache` }];
      }
      return [job, { run: false, reason: passedThere }];
    }),
  ) as Decision;
  return { ...byPath, decision, tested };
}

/**
 * Files that change the key of the Rust build cache (Swatinem/rust-cache v2.9.2, README "Cache
 * Details"): every Cargo.lock and Cargo.toml, the root toolchain file and Cargo configuration, and
 * the job's id and environment (ci.yml, and the local actions, which can set compiler variables).
 * ci.yml saves that cache only on main (`save-if`), and rust-cache saves only when the key
 * changed ("Cache up-to-date." otherwise). So when a merge changes none of these, skipping `rust`
 * on main loses no cache; when it changes one, `rust` runs on main to save the new cache.
 * The key also holds rustc's version and hash: rustup installs what rust-toolchain.toml names, an
 * exact version, so they change only with that file. workflow-conditions.test.ts fails when the
 * rust-cache step gets an input that changes the key, or when the toolchain channel floats.
 */
export function affectsRustCacheKey(path: string): boolean {
  const name = posix.basename(path);
  return (
    name === "Cargo.lock" ||
    name === "Cargo.toml" ||
    path === "rust-toolchain" ||
    path === "rust-toolchain.toml" ||
    path.startsWith(".cargo/") ||
    path === CI_WORKFLOW_PATH ||
    path.startsWith(".github/actions/")
  );
}

/** Decides for the current workflow run; never throws, every problem runs everything. */
export async function plan(root: string, env: Record<string, string | undefined>, github: GitHubReads): Promise<Plan> {
  const everything = (reason: string): Plan => ({ decision: runEverything(reason), files: [], everything: reason });
  const message = (error: unknown) => (error instanceof Error ? error.message : String(error));
  try {
    const eventName = env.GITHUB_EVENT_NAME ?? "";
    if (eventName === "push") {
      return await planPush(root, "HEAD", env.GITHUB_REPOSITORY ?? "", github);
    }
    if (eventName !== "pull_request") {
      return everything(`the \`${eventName || "unknown"}\` event is not compared by path`);
    }
    const range = pullRequestRange(root);
    if ("everything" in range) {
      return everything(range.everything);
    }
    const files = changedFiles(root, range.base, range.head);
    return { decision: decide(files), files, base: `${range.base} (the tip of the base branch (first parent of the merge commit))` };
  } catch (error) {
    return everything(`the changed files cannot be determined: ${message(error)}`);
  }
}

/** `key=value` lines for GITHUB_OUTPUT. */
export function outputLines(decision: Decision): string {
  return PATH_JOBS.map((job) => `${job}=${decision[job].run}\n`).join("");
}

const SUMMARY_FILE_LIMIT = 200;

const cell = (value: string) => value.replaceAll("|", "\\|");

/** The evidence of the tested-tree check, as Markdown lines. */
function testedTreeLines(tested: TestedTree<PathJob>, decision: Decision): string[] {
  const lines = ["### Same tree as the merged pull request", ""];
  if (!tested.applies) {
    lines.push(`Not used: ${tested.reason}.`);
    return lines;
  }
  const runs = tested.runs.map((run) => (run.url === "" ? `${run.id}` : `[${run.id}](${run.url})`)).join(", ");
  lines.push(
    `The pushed tree \`${tested.tree}\` is the tree of \`${tested.head}\`, the last head of pull request #${tested.pullRequest}, ` +
      `which contains the pushed commit's parent \`${tested.parent}\`; so every ci run of that head tested this tree. ` +
      `Run(s) of that head: ${runs}, all successful. Since ${tested.forwardSince}, main was not force-pushed and held only commits in the pushed commit's history.`,
    "",
    "| job | in those runs |",
    "| --- | --- |",
  );
  for (const job of PATH_JOBS) {
    const there = !tested.passed.includes(job)
      ? `${tested.notPassed[job] ?? "?"}: path rules decide`
      : TIME_DEPENDENT_JOBS.has(job)
        ? "ran and passed, but its result changes with time: path rules decide"
        : decision[job].run
        ? "ran and passed, but runs here (see why above)"
        : "ran and passed: skipped here";
    lines.push(`| \`${job}\` | ${cell(there)} |`);
  }
  return lines;
}

/** The job summary: what runs and why, the tested-tree evidence, then the changed files. */
export function summary(result: Plan): string {
  const lines = ["## Path-dependent jobs", "", "| job | runs | why |", "| --- | --- | --- |"];
  for (const job of PATH_JOBS) {
    const { run, reason } = result.decision[job];
    lines.push(`| \`${job}\` | ${run ? "yes" : "skipped"} | ${cell(reason)} |`);
  }
  lines.push("", "`gitleaks` and `workflow-policy` always run.", "");
  if (result.tested !== undefined) {
    lines.push(...testedTreeLines(result.tested, result.decision), "");
  }
  if (result.everything === undefined) {
    lines.push(`Compared with ${result.base ?? "?"}.`, "", `### Changed files (${result.files.length})`, "");
    for (const file of result.files.slice(0, SUMMARY_FILE_LIMIT)) {
      lines.push(`- \`${file}\``);
    }
    if (result.files.length > SUMMARY_FILE_LIMIT) {
      lines.push(`- ... and ${result.files.length - SUMMARY_FILE_LIMIT} more`);
    }
  } else {
    lines.push(`Path rules not compared: ${result.everything}.`);
  }
  return `${lines.join("\n")}\n`;
}

/**
 * Prints the decision to the log. In Actions, the reason the path rules were not compared is a
 * notice annotation; `--explain` prints it as a plain line.
 */
export function report(result: Plan, actions = true, log: (line: string) => void = console.log): void {
  if (result.everything !== undefined) {
    // A notice, not a warning: running every job is always a safe answer.
    const line = `Path rules not compared, they run every job: ${result.everything.replaceAll("\n", " ")}`;
    log(actions ? `::notice title=changes::${line}` : line);
  }
  if (result.base !== undefined) {
    log(`compared with ${result.base}: ${result.files.length} changed file(s)`);
  }
  if (result.tested !== undefined) {
    log(
      result.tested.applies
        ? `same tree as pull request #${result.tested.pullRequest} (head ${result.tested.head}, tree ${result.tested.tree}, ` +
            `run(s) ${result.tested.runs.map((run) => run.id).join(", ")}); passed there: ${result.tested.passed.join(", ") || "none"}`
        : `same-tree check not used: ${result.tested.reason}`,
    );
  }
  for (const job of PATH_JOBS) {
    log(`${job}: ${result.decision[job].run ? "runs" : "skipped"} (${result.decision[job].reason})`);
  }
}

/** `--explain <commit>`: the decision a push of that commit to main gets, from real data. */
export async function explain(root: string, commit: string, env: Record<string, string | undefined>, github: GitHubReads): Promise<Plan> {
  const repository = env.GITHUB_REPOSITORY || DEFAULT_REPOSITORY;
  try {
    return await planPush(root, commit, repository, github);
  } catch (error) {
    const reason = `the changed files cannot be determined: ${error instanceof Error ? error.message : String(error)}`;
    return { decision: runEverything(reason), files: [], everything: reason };
  }
}

export async function main(env: Record<string, string | undefined>, args: readonly string[] = []): Promise<number> {
  const root = resolve(import.meta.dir, "..", "..");
  if (args.length > 0) {
    if (args.length !== 2 || args[0] !== "--explain" || !/^[0-9a-f]{7,40}$/.test(args[1] ?? "")) {
      console.error("usage: bun scripts/ci/changes.ts [--explain <commit on main>]");
      return 2;
    }
    const repository = env.GITHUB_REPOSITORY || DEFAULT_REPOSITORY;
    const result = await explain(root, args[1] as string, env, githubReads({ ...env, GITHUB_REPOSITORY: repository }));
    console.log(`decision for a push of ${args[1]} to ${repository}:main (read-only; nothing is written)`);
    report(result, false);
    console.log("");
    console.log(summary(result));
    return 0;
  }
  const result = await plan(root, env, githubReads(env));
  report(result);
  if (env.GITHUB_OUTPUT) {
    appendFileSync(env.GITHUB_OUTPUT, outputLines(result.decision));
  }
  if (env.GITHUB_STEP_SUMMARY) {
    appendFileSync(env.GITHUB_STEP_SUMMARY, summary(result));
  }
  return 0;
}

if (import.meta.main) {
  process.exit(await main(process.env, Bun.argv.slice(2)));
}
