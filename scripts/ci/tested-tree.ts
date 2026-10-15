/**
 * Which path-dependent ci.yml jobs a push to main may skip because the pushed tree already passed
 * them in the merged pull request's own ci run (Laysi/trustiot-v2#431; docs/architecture/ci-release.md,
 * section "依變更路徑決定 job"). `changes.ts` calls `testedTree` for every push to main and falls
 * back to its path rules for every job this does not skip.
 *
 * Trust: a pull request run executes the pull request's own ci.yml and scripts, so nothing that
 * run produced (artifacts, summaries, check-run text, logs, outputs) is read here. The decision
 * uses only GitHub API metadata (the pull request a main commit came from, its head, its base
 * changes, the runs of ci.yml for that head and the conclusions of their jobs in every attempt,
 * a commit's tree, a merge base, main's push runs and force pushes) and git on the pushed commit.
 * Every condition below must hold; anything else, a failed request included, skips nothing:
 *
 * 1. The pushed commit C has exactly one parent P (a squash merge), and P has a push run of ci.yml
 *    on main of its own, so main moved P -> C in one push.
 * 2. GitHub lists exactly one merged pull request N whose merge commit is C, based on main of this
 *    repository; its last head is H, on branch `ref` of repository `repo` (a deleted fork: no).
 *    The pull requests (any state) whose head is `repo`'s `ref` are exactly N.
 * 3. N's base branch never changed (no base_ref_changed or automatic_base_change_* event): a
 *    `pull_request` run is triggered only by opened, synchronize and reopened, so a base edit
 *    starts no run, and an earlier run of H may have tested a merge into another branch.
 * 4. P is an ancestor of H (merge base of P and H is P).
 * 5. tree(C) equals tree(H), and that ci.yml triggers on `pull_request` without filters.
 * 6. The counted runs: the `pull_request` runs of ci.yml whose head is H, on `repo`'s `ref`. There
 *    is at least one, and every one of them completed with success. With condition 2 they are N's.
 * 7. Main only moved forward while those runs could have been made. GitHub created P (its
 *    committer is GitHub and GitHub's signature on it is verified, as on every squash merge made
 *    on GitHub), so P's committer date is when it came to exist; H contains P, so H was pushed,
 *    and every test merge commit of H built, after that. From an hour before that date on, GitHub records no force push to main, and every push run of ci.yml on main is for a
 *    commit in C's history (or a later commit on top of C); the listed push runs reach back past
 *    that time. So every main tip GitHub merged H into was C's ancestor, that is P or P's ancestor
 *    (C has one parent), and so H's ancestor (condition 4). Merging an ancestor into H gives
 *    tree(H) whatever the merge algorithm, so every counted run, re-runs included (a re-run uses
 *    the original run's commit), tested tree(H) = tree(C), with the same ci.yml and scripts. No
 *    bound on GitHub's delays is assumed: the time comes from what H contains.
 * 8. Per job: a job is skipped only if, in every counted run, the latest attempt has exactly one
 *    job with that name, and every attempt's job with that name concluded success (a job re-run
 *    until it passed does not count). A job those runs skipped by their path rules, or that is
 *    missing, keeps the path rules on main, compared with the newest green main commit.
 */

import { CI_WORKFLOW_PATH, newestMainCiRun, parseWorkflowRuns, type WorkflowRun } from "../release/lib.ts";

const COMMIT_SHA = /^[0-9a-f]{40}$/;

/** The branch whose pushes this compares. */
export const MAIN_BRANCH = "main";

/** Issue event types that mean a pull request's base branch changed (REST "Issue event types"). */
export const BASE_CHANGE_EVENTS: ReadonlySet<string> = new Set([
  "base_ref_changed",
  "automatic_base_change_succeeded",
  "automatic_base_change_failed",
]);

/**
 * How long before GitHub created P main's history is checked (condition 7): a margin for the
 * difference between the clock that wrote P's committer date and the one that times runs and
 * force pushes. It does not bound any delay: nothing that contains P can be older than P.
 */
export const HISTORY_MARGIN_MS = 60 * 60 * 1000;

/** The fields of a pull request that the decision reads. */
export interface PullRequest {
  number: number;
  /** Null until merged. */
  mergedAt: string | null;
  /** After a squash merge, the squashed commit on the base branch. */
  mergeCommitSha: string;
  headSha: string;
  /** The head branch's name. */
  headRef: string;
  /** `owner/name` of the head repository; null when it was deleted. */
  headRepository: string | null;
  baseRef: string;
  /** `owner/name` of the base repository. */
  baseRepository: string;
}

/** A workflow run with the fields beyond release/lib.ts's WorkflowRun that the decision reads. */
export interface Run extends WorkflowRun {
  /** When the run was created (ISO 8601); a re-run keeps it. */
  createdAt: string;
  /** The latest attempt, 1 for a run that was never re-run. */
  runAttempt: number;
  /** `owner/name` of the repository of the run's head branch; null when GitHub reports none. */
  headRepository: string | null;
}

/** The fields of a workflow job that the decision reads. */
export interface WorkflowJob {
  name: string;
  status: string;
  /** Empty while the job has not finished. */
  conclusion: string;
  /** The attempt of the run this job belongs to. */
  runAttempt: number;
}

/** The GitHub REST reads behind the decision; injected so that tests need no network. */
export interface GitHubReads {
  /** The newest push runs of ci.yml on main (actions: read). */
  mainRuns(): Promise<Run[]>;
  /** GET /repos/{repo}/commits/{sha}/pulls (pull-requests: read). */
  pullsOfCommit(commit: string): Promise<PullRequest[]>;
  /** The numbers of GET /repos/{repo}/pulls?state=all&head={owner}:{ref} (pull-requests: read). */
  pullsWithHead(owner: string, ref: string): Promise<number[]>;
  /** The event types of GET /repos/{repo}/issues/{number}/events, every page (issues or pull-requests: read). */
  issueEventTypes(number: number): Promise<string[]>;
  /** The merge base of GET /repos/{repo}/compare/{base}...{head} (contents: read). */
  mergeBase(base: string, head: string): Promise<string>;
  /** The tree of GET /repos/{repo}/git/commits/{sha} (contents: read). */
  treeOf(commit: string): Promise<string>;
  /** The pull_request runs of ci.yml whose head is `head` (actions: read). */
  pullRequestRuns(head: string): Promise<Run[]>;
  /**
   * When GitHub itself created `commit`: its committer date, if GET /repos/{repo}/git/commits/{sha}
   * (contents: read) shows GitHub (noreply@github.com) as its committer and a verified signature;
   * null otherwise. GitHub signs what it commits, so then the date is the one GitHub wrote.
   */
  createdByGitHub(commit: string): Promise<string | null>;
  /** The jobs of every attempt of a run (actions: read). */
  jobsOf(runId: number): Promise<WorkflowJob[]>;
  /**
   * The times of the newest force pushes to main, newest first: one page of GET
   * /repos/{repo}/activity (contents: read). The newest is enough to tell whether one happened
   * after a given time.
   */
  forcePushesToMain(): Promise<string[]>;
}

/** Whether the push may skip jobs that passed on the same tree, with the evidence. */
export type TestedTree<Job extends string = string> =
  | {
      applies: true;
      pullRequest: number;
      /** The pull request's last head, whose tree the pushed commit has. */
      head: string;
      /** The pushed commit's first and only parent, an ancestor of `head`. */
      parent: string;
      tree: string;
      runs: { id: number; url: string }[];
      /** Since when main is known to have only moved forward (ISO 8601). */
      forwardSince: string;
      /** The jobs that ran and passed in every attempt of every run. */
      passed: Job[];
      /** Why each other job is not skipped by this. */
      notPassed: Partial<Record<Job, string>>;
    }
  | { applies: false; reason: string };

function git(root: string, args: readonly string[]): string {
  const result = Bun.spawnSync(["git", ...args], { cwd: root, stdout: "pipe", stderr: "pipe" });
  if (result.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr.toString().trim()}`);
  }
  return result.stdout.toString();
}

/** Whether `ancestor` is `commit` or one of its ancestors; false for a commit git does not have. */
function isAncestor(root: string, ancestor: string, commit: string): boolean {
  if (!COMMIT_SHA.test(ancestor) || !COMMIT_SHA.test(commit)) {
    return false;
  }
  return Bun.spawnSync(["git", "merge-base", "--is-ancestor", ancestor, commit], { cwd: root, stdout: "ignore", stderr: "ignore" }).exitCode === 0;
}

const short = (commit: string) => commit.slice(0, 12);
const isCiWorkflow = (run: WorkflowRun) => run.path === CI_WORKFLOW_PATH || run.path.startsWith(`${CI_WORKFLOW_PATH}@`);
const sameRepository = (a: string | null, b: string | null) => a !== null && b !== null && a.toLowerCase() === b.toLowerCase();

/** Milliseconds since the epoch of an ISO 8601 time from the API; throws on anything else. */
function time(value: string, what: string): number {
  const parsed = Date.parse(value);
  if (!/^\d{4}-\d{2}-\d{2}T/.test(value) || Number.isNaN(parsed)) {
    throw new Error(`${what} has no valid time ("${value}")`);
  }
  return parsed;
}

/**
 * Why `workflow` (ci.yml's text) does not run on every opened, synchronize and reopened event of
 * every pull request, or undefined when it does: an unfiltered `pull_request` trigger. The
 * pushed tree's copy is the one the pull request's runs used, because the trees are equal.
 */
export function pullRequestTriggerProblem(workflow: string): string | undefined {
  let doc: unknown;
  try {
    doc = Bun.YAML.parse(workflow);
  } catch {
    return "cannot be parsed";
  }
  const on = isMap(doc) ? doc.on : undefined;
  // `on: pull_request` and `on: [pull_request, ...]` name the event without filters.
  if (on === "pull_request" || (Array.isArray(on) && on.includes("pull_request"))) {
    return undefined;
  }
  if (!isMap(on) || !Object.hasOwn(on, "pull_request")) {
    return "has no pull_request trigger";
  }
  const filters = on.pull_request;
  if (filters !== null && !(isMap(filters) && Object.keys(filters).length === 0)) {
    return "filters its pull_request trigger";
  }
  return undefined;
}

/**
 * Why `job` did not pass in `run`, or undefined when it did: exactly one job of that name in the
 * latest attempt, and every attempt's job of that name completed with success.
 */
function jobProblem(job: string, run: Run, runJobs: readonly WorkflowJob[]): string | undefined {
  const named = runJobs.filter((candidate) => candidate.name === job);
  const latest = named.filter((candidate) => candidate.runAttempt === run.runAttempt);
  if (latest.length !== 1) {
    return `${latest.length === 0 ? "no job" : `${latest.length} jobs`} of that name in attempt ${run.runAttempt} of run ${run.id}`;
  }
  for (const candidate of named.toSorted((a, b) => a.runAttempt - b.runAttempt)) {
    const state = candidate.status !== "completed" ? candidate.status || "not finished" : candidate.conclusion || "no conclusion";
    if (state !== "success") {
      return `${state} in ${run.runAttempt > 1 ? `attempt ${candidate.runAttempt} of ` : ""}run ${run.id}`;
    }
  }
  return undefined;
}

/**
 * Decides which of `jobs` the push of `commit` to main may skip because the identical tree passed
 * them in the merged pull request's runs. Never throws: every problem means "skip nothing".
 */
export async function testedTree<Job extends string>(
  root: string,
  commit: string,
  repository: string,
  jobs: readonly Job[],
  mainRuns: readonly Run[],
  github: GitHubReads,
): Promise<TestedTree<Job>> {
  const no = (reason: string): TestedTree<Job> => ({ applies: false, reason });
  try {
    // 1. One parent, which main held on its own.
    const [pushed = "", ...parents] = git(root, ["rev-list", "--parents", "-n", "1", commit]).trim().split(" ");
    if (parents.length !== 1) {
      return no(`the pushed commit has ${parents.length} parents; only a commit with one parent (a squash merge) is compared`);
    }
    const parent = parents[0] as string;
    if (newestMainCiRun(mainRuns, parent) === undefined) {
      return no(
        `its parent ${short(parent)} has no push run of ci.yml on main, so main did not move to the pushed commit from its parent in one push`,
      );
    }

    // 2. The one merged pull request, and the one pull request with its head branch.
    const merged = (await github.pullsOfCommit(pushed)).filter((pr) => pr.mergedAt !== null && pr.mergeCommitSha === pushed);
    if (merged.length !== 1) {
      return no(`${merged.length} merged pull requests have the pushed commit as their merge commit; exactly one is needed`);
    }
    const pr = merged[0] as PullRequest;
    if (pr.baseRef !== MAIN_BRANCH || pr.baseRepository.toLowerCase() !== repository.toLowerCase()) {
      return no(`pull request #${pr.number} was merged into ${pr.baseRepository}:${pr.baseRef}, not ${repository}:${MAIN_BRANCH}`);
    }
    if (!COMMIT_SHA.test(pr.headSha)) {
      return no(`pull request #${pr.number} has no valid head commit`);
    }
    if (pr.headRepository === null || pr.headRef === "") {
      return no(`the head repository or branch of pull request #${pr.number} is gone, so its runs cannot be told from other pull requests'`);
    }
    const head = pr.headSha;
    const headLabel = `${pr.headRepository}:${pr.headRef}`;
    const owner = pr.headRepository.split("/")[0] ?? "";
    const sameHead = await github.pullsWithHead(owner, pr.headRef);
    if (sameHead.length !== 1 || sameHead[0] !== pr.number) {
      return no(
        `the pull requests with the head branch ${headLabel} are ${sameHead.length === 0 ? "none" : sameHead.map((n) => `#${n}`).join(", ")}, ` +
          `not exactly #${pr.number}, so a run of that branch may belong to another pull request`,
      );
    }

    // 3. The base never changed.
    const baseChanges = (await github.issueEventTypes(pr.number)).filter((type) => BASE_CHANGE_EVENTS.has(type));
    if (baseChanges.length > 0) {
      return no(
        `the base branch of pull request #${pr.number} was changed (${[...new Set(baseChanges)].join(", ")}), ` +
          "so a run of its head may have tested a merge into another branch",
      );
    }

    // 4. The head contains the parent.
    const mergeBase = await github.mergeBase(parent, head);
    if (mergeBase !== parent) {
      return no(
        `the head ${short(head)} of pull request #${pr.number} does not contain the pushed commit's parent ${short(parent)}, ` +
          "so the base its runs tested cannot be confirmed",
      );
    }

    // 5. Same tree, unfiltered trigger.
    const tree = git(root, ["rev-parse", `${pushed}^{tree}`]).trim();
    const headTree = await github.treeOf(head);
    if (headTree !== tree) {
      return no(`the pushed tree ${short(tree)} differs from the tree ${short(headTree)} of the head ${short(head)} of pull request #${pr.number}`);
    }
    const trigger = pullRequestTriggerProblem(git(root, ["show", `${pushed}:${CI_WORKFLOW_PATH}`]));
    if (trigger !== undefined) {
      return no(`the pushed ${CI_WORKFLOW_PATH} ${trigger}, so not every event of the pull request is known to have run it`);
    }

    // 6. The pull request's runs of its head, all successful.
    const runs = (await github.pullRequestRuns(head)).filter(
      (run) =>
        isCiWorkflow(run) &&
        run.event === "pull_request" &&
        run.headSha === head &&
        run.headBranch === pr.headRef &&
        sameRepository(run.headRepository, pr.headRepository),
    );
    if (runs.length === 0) {
      return no(`no pull_request run of ci.yml has the head ${short(head)} on ${headLabel} of pull request #${pr.number}`);
    }
    const unfinished = runs.find((run) => run.status !== "completed" || run.conclusion !== "success");
    if (unfinished !== undefined) {
      const state = unfinished.status !== "completed" ? unfinished.status || "not finished" : unfinished.conclusion || "no conclusion";
      return no(`run ${unfinished.id} of the head ${short(head)} of pull request #${pr.number} is ${state}, not success`);
    }

    // 7. Main only moved forward since GitHub built the merge commits those runs tested. They all
    // contain H, which contains P, so none of them is older than P; GitHub's signed committer date
    // says when GitHub created P. No delay of GitHub's (push to run, merge commit to run) matters.
    const created = await github.createdByGitHub(parent);
    if (created === null) {
      return no(
        `its parent ${short(parent)} is not a commit GitHub created and signed, ` +
          "so the earliest time the pull request's runs could have tested a merge is unknown",
      );
    }
    const since = time(created, `the parent ${short(parent)}`) - HISTORY_MARGIN_MS;
    const forwardSince = new Date(since).toISOString();
    const pushRuns = mainRuns.filter((run) => isCiWorkflow(run) && run.event === "push" && run.headBranch === MAIN_BRANCH);
    const oldest = Math.min(...pushRuns.map((run) => time(run.createdAt, `run ${run.id}`)));
    if (!(oldest < since)) {
      return no(`the listed push runs of ci.yml on main do not reach back to ${forwardSince}, so main's history since then is unknown`);
    }
    const elsewhere = pushRuns.find(
      (run) => time(run.createdAt, `run ${run.id}`) >= since && !isAncestor(root, run.headSha, pushed) && !isAncestor(root, pushed, run.headSha),
    );
    if (elsewhere !== undefined) {
      return no(
        `main held ${short(elsewhere.headSha)} (push run ${elsewhere.id}), which is not in the pushed commit's history, ` +
          `after ${forwardSince}: a pull request run may have tested a merge into it`,
      );
    }
    const forcePush = (await github.forcePushesToMain()).find((at) => time(at, "a force push") >= since);
    if (forcePush !== undefined) {
      return no(`main was force-pushed at ${forcePush}, after ${forwardSince}: a pull request run may have tested a merge into a commit that main no longer holds`);
    }

    // 8. Per job, every attempt of every run.
    const passed: Job[] = [];
    const notPassed: Partial<Record<Job, string>> = {};
    const jobsByRun = await Promise.all(runs.map(async (run) => ({ run, jobs: await github.jobsOf(run.id) })));
    for (const job of jobs) {
      const problem = jobsByRun.map(({ run, jobs: runJobs }) => jobProblem(job, run, runJobs)).find((reason) => reason !== undefined);
      if (problem === undefined) {
        passed.push(job);
      } else {
        notPassed[job] = problem;
      }
    }
    return {
      applies: true,
      pullRequest: pr.number,
      head,
      parent,
      tree,
      runs: runs.map((run) => ({ id: run.id, url: run.url })).toSorted((a, b) => a.id - b.id),
      forwardSince,
      passed,
      notPassed,
    };
  } catch (error) {
    return no(`the pull request's runs cannot be checked: ${error instanceof Error ? error.message : String(error)}`);
  }
}

// ---------------------------------------------------------------------------------------------
// The GitHub REST answers (the calls are in changes.ts, githubReads)

export function isMap(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/** `full_name` of a repository object, or null. */
function fullName(repo: unknown): string | null {
  return isMap(repo) && typeof repo.full_name === "string" && repo.full_name !== "" ? repo.full_name : null;
}

/** Reads the answer of GET /repos/{repo}/commits/{sha}/pulls. */
export function parsePullRequests(body: unknown): PullRequest[] {
  if (!Array.isArray(body)) {
    throw new Error("the GitHub API did not answer with a list of pull requests");
  }
  return body.map((item: unknown, index): PullRequest => {
    const head = isMap(item) ? item.head : undefined;
    const base = isMap(item) ? item.base : undefined;
    const baseRepo = isMap(base) ? base.repo : undefined;
    if (!isMap(item) || typeof item.number !== "number" || !isMap(head) || !isMap(base) || !isMap(baseRepo)) {
      throw new Error(`pull request ${index} in the GitHub API answer lacks number, head, base or base.repo`);
    }
    return {
      number: item.number,
      mergedAt: typeof item.merged_at === "string" ? item.merged_at : null,
      mergeCommitSha: text(item.merge_commit_sha),
      headSha: text(head.sha),
      headRef: text(head.ref),
      headRepository: fullName(head.repo),
      baseRef: text(base.ref),
      baseRepository: text(baseRepo.full_name),
    };
  });
}

/** The pull request numbers of GET /repos/{repo}/pulls. */
export function parsePullNumbers(body: unknown): number[] {
  if (!Array.isArray(body)) {
    throw new Error("the GitHub API did not answer with a list of pull requests");
  }
  return body.map((item: unknown, index): number => {
    if (!isMap(item) || typeof item.number !== "number") {
      throw new Error(`pull request ${index} in the GitHub API answer lacks its number`);
    }
    return item.number;
  });
}

/**
 * Reads a list-workflow-runs answer with the fields the decision needs beyond WorkflowRun. With
 * `complete`, an answer that does not fit in the page is an error.
 */
export function parseRuns(body: unknown, complete: boolean): Run[] {
  const runs = parseWorkflowRuns(body);
  const raw = (isMap(body) && Array.isArray(body.workflow_runs) ? body.workflow_runs : []) as unknown[];
  if (complete && (!isMap(body) || typeof body.total_count !== "number" || body.total_count > runs.length)) {
    throw new Error("the runs of ci.yml do not fit in one page");
  }
  return runs.map((run, index): Run => {
    const item = raw[index];
    if (!isMap(item) || typeof item.created_at !== "string" || typeof item.run_attempt !== "number") {
      throw new Error(`workflow run ${index} in the GitHub API answer lacks created_at or run_attempt`);
    }
    return { ...run, createdAt: item.created_at, runAttempt: item.run_attempt, headRepository: fullName(item.head_repository) };
  });
}

/** Reads the answer of GET /repos/{repo}/actions/runs/{id}/jobs?filter=all. */
export function parseJobs(body: unknown): WorkflowJob[] {
  if (!isMap(body) || !Array.isArray(body.jobs)) {
    throw new Error("the GitHub API answered without a jobs list");
  }
  if (typeof body.total_count === "number" && body.total_count > body.jobs.length) {
    throw new Error(`the run has ${body.total_count} jobs, more than one page`);
  }
  return body.jobs.map((job: unknown, index): WorkflowJob => {
    if (!isMap(job) || typeof job.name !== "string" || typeof job.run_attempt !== "number") {
      throw new Error(`job ${index} in the GitHub API answer lacks a name or run_attempt`);
    }
    return { name: job.name, status: text(job.status), conclusion: text(job.conclusion), runAttempt: job.run_attempt };
  });
}

/** The event types of one page of GET /repos/{repo}/issues/{number}/events. */
export function parseIssueEventTypes(body: unknown): string[] {
  if (!Array.isArray(body)) {
    throw new Error("the GitHub API did not answer with a list of issue events");
  }
  return body.map((event: unknown, index): string => {
    if (!isMap(event) || typeof event.event !== "string") {
      throw new Error(`issue event ${index} in the GitHub API answer lacks its type`);
    }
    return event.event;
  });
}

/** The times of the force pushes in one page of GET /repos/{repo}/activity. */
export function parseForcePushTimes(body: unknown): string[] {
  if (!Array.isArray(body)) {
    throw new Error("the GitHub API did not answer with a list of repository activities");
  }
  return body.map((activity: unknown, index): string => {
    if (!isMap(activity) || activity.activity_type !== "force_push" || typeof activity.timestamp !== "string") {
      throw new Error(`activity ${index} in the GitHub API answer is not a force push with a timestamp`);
    }
    return activity.timestamp;
  });
}

/** The merge base in the answer of GET /repos/{repo}/compare/{base}...{head}. */
export function parseMergeBase(body: unknown): string {
  const mergeBase = isMap(body) && isMap(body.merge_base_commit) ? text(body.merge_base_commit.sha) : "";
  if (!COMMIT_SHA.test(mergeBase)) {
    throw new Error("the comparison has no merge base commit");
  }
  return mergeBase;
}

/** The tree in the answer of GET /repos/{repo}/git/commits/{commit}, checked to be that commit's. */
/** The committer email of the commits GitHub itself creates (squash merges, web edits). */
export const GITHUB_COMMITTER_EMAIL = "noreply@github.com";

/**
 * Reads the answer of GET /repos/{repo}/git/commits/{sha}: the committer date when GitHub created
 * and signed the commit, null when someone else did.
 */
export function parseGitHubCreated(body: unknown, commit: string): string | null {
  if (!isMap(body) || body.sha !== commit || !isMap(body.committer) || !isMap(body.verification)) {
    throw new Error(`the commit ${commit} came back without its committer or verification`);
  }
  const { email, date } = body.committer;
  if (body.verification.verified !== true || email !== GITHUB_COMMITTER_EMAIL) {
    return null;
  }
  if (typeof date !== "string") {
    throw new Error(`the commit ${commit} came back without its committer date`);
  }
  return date;
}

export function parseCommitTree(body: unknown, commit: string): string {
  const tree = isMap(body) && isMap(body.tree) ? text(body.tree.sha) : "";
  if (!isMap(body) || body.sha !== commit || !COMMIT_SHA.test(tree)) {
    throw new Error(`the commit ${commit} came back without its tree`);
  }
  return tree;
}
