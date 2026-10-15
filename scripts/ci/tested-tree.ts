/**
 * Which path-dependent ci.yml jobs a push to main may skip because the pushed tree already passed
 * them in the merged pull request's own ci run (Laysi/trustiot-v2#431; docs/architecture/ci-release.md,
 * section "依變更路徑決定 job"). `changes.ts` calls `testedTree` for every push to main and falls
 * back to its path rules for every job this does not skip.
 *
 * Trust: a pull request run executes the pull request's own ci.yml and scripts, so nothing that
 * run produced (artifacts, summaries, check-run text, logs, outputs) is read here. The decision
 * uses only GitHub API metadata (the pull request a main commit came from, its head, its base
 * changes, the runs of ci.yml for that head and the conclusions of their jobs, a commit's tree and
 * a merge base) and git on the checked-out pushed commit. Every condition below must hold;
 * anything else, a failed request included, skips nothing:
 *
 * 1. The pushed commit C has exactly one parent P (a squash merge), and P has a push run of ci.yml
 *    on main of its own, so main moved P -> C in one push.
 * 2. GitHub lists exactly one merged pull request N whose merge commit is C, based on main of this
 *    repository; its last head is H.
 * 3. N's base branch never changed (no base_ref_changed or automatic_base_change_* event): a
 *    `pull_request` run is triggered only by opened, synchronize and reopened, so a base edit
 *    starts no run, and an earlier run of H may have tested a merge into another branch.
 * 4. P is an ancestor of H (merge base of P and H is P). Every main tip before C is P or an
 *    ancestor of P (C has one parent, main only moves forward), so whatever main was when GitHub
 *    built N's test merge commit for H, it was an ancestor of H, and merging an ancestor into H
 *    gives H's tree whatever the merge algorithm. So every run of N for head H tested tree(H),
 *    reruns included (a re-run uses the original run's commit), and the merge can never conflict,
 *    so every opened, synchronize or reopened event of N with head H created a run.
 * 5. tree(C) equals tree(H): main now holds exactly the tree those runs tested, including the same
 *    ci.yml and scripts. That ci.yml triggers on `pull_request` without filters, so it ran for
 *    every opened, synchronize and reopened event of N (condition 4).
 * 6. There is at least one `pull_request` run of ci.yml whose head is H, and every one of them
 *    (N's and any other pull request's with the same head) completed with success. The API does
 *    not say which pull request a run belongs to once N is closed, so all of them must pass.
 * 7. Per job: a job is skipped only if, in every one of those runs, the job with that name ran
 *    and concluded success. A job those runs skipped by their path rules (or that is missing)
 *    keeps the path rules on main, compared with the newest green main commit.
 */

import { CI_WORKFLOW_PATH, newestMainCiRun, type WorkflowRun } from "../release/lib.ts";

const COMMIT_SHA = /^[0-9a-f]{40}$/;

/** The branch whose pushes this compares. */
export const MAIN_BRANCH = "main";

/** Issue event types that mean a pull request's base branch changed (REST "Issue event types"). */
export const BASE_CHANGE_EVENTS: ReadonlySet<string> = new Set([
  "base_ref_changed",
  "automatic_base_change_succeeded",
  "automatic_base_change_failed",
]);

/** The fields of a pull request that the decision reads. */
export interface PullRequest {
  number: number;
  /** Null until merged. */
  mergedAt: string | null;
  /** After a squash merge, the squashed commit on the base branch. */
  mergeCommitSha: string;
  headSha: string;
  baseRef: string;
  /** `owner/name` of the base repository. */
  baseRepository: string;
}

/** The fields of a workflow job that the decision reads. */
export interface WorkflowJob {
  name: string;
  status: string;
  /** Empty while the job has not finished. */
  conclusion: string;
}

/** The GitHub REST reads behind the decision; injected so that tests need no network. */
export interface GitHubReads {
  /** The newest push runs of ci.yml on main (actions: read). */
  mainRuns(): Promise<WorkflowRun[]>;
  /** GET /repos/{repo}/commits/{sha}/pulls (pull-requests: read). */
  pullsOfCommit(commit: string): Promise<PullRequest[]>;
  /** The event types of GET /repos/{repo}/issues/{number}/events, every page (issues or pull-requests: read). */
  issueEventTypes(number: number): Promise<string[]>;
  /** The merge base of GET /repos/{repo}/compare/{base}...{head} (contents: read). */
  mergeBase(base: string, head: string): Promise<string>;
  /** The tree of GET /repos/{repo}/git/commits/{sha} (contents: read). */
  treeOf(commit: string): Promise<string>;
  /** The pull_request runs of ci.yml whose head is `head` (actions: read). */
  pullRequestRuns(head: string): Promise<WorkflowRun[]>;
  /** The jobs of the latest attempt of a run (actions: read). */
  jobsOf(runId: number): Promise<WorkflowJob[]>;
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
      /** The jobs that ran and passed in every run. */
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

const short = (commit: string) => commit.slice(0, 12);

/**
 * Why `workflow` (ci.yml's text) does not run on every opened, synchronize and reopened event of
 * every pull request, or undefined when it does: an unfiltered `pull_request` trigger. The
 * argument that the pull request's own runs are among the runs of its head relies on it; the
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

/** Why `job` did not run and pass in `runJobs`, or undefined when it did exactly once. */
function jobProblem(job: string, runJobs: readonly WorkflowJob[]): string | undefined {
  const named = runJobs.filter((candidate) => candidate.name === job);
  if (named.length !== 1) {
    return named.length === 0 ? "no job of that name" : `${named.length} jobs of that name`;
  }
  const only = named[0] as WorkflowJob;
  if (only.status !== "completed") {
    return only.status || "not finished";
  }
  return only.conclusion === "success" ? undefined : only.conclusion || "no conclusion";
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
  mainRuns: readonly WorkflowRun[],
  github: GitHubReads,
): Promise<TestedTree<Job>> {
  const no = (reason: string): TestedTree<Job> => ({ applies: false, reason });
  try {
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
    const head = pr.headSha;

    const baseChanges = (await github.issueEventTypes(pr.number)).filter((type) => BASE_CHANGE_EVENTS.has(type));
    if (baseChanges.length > 0) {
      return no(
        `the base branch of pull request #${pr.number} was changed (${[...new Set(baseChanges)].join(", ")}), ` +
          "so a run of its head may have tested a merge into another branch",
      );
    }

    const mergeBase = await github.mergeBase(parent, head);
    if (mergeBase !== parent) {
      return no(
        `the head ${short(head)} of pull request #${pr.number} does not contain the pushed commit's parent ${short(parent)}, ` +
          "so the base its runs tested cannot be confirmed",
      );
    }

    const tree = git(root, ["rev-parse", `${pushed}^{tree}`]).trim();
    const headTree = await github.treeOf(head);
    if (headTree !== tree) {
      return no(`the pushed tree ${short(tree)} differs from the tree ${short(headTree)} of the head ${short(head)} of pull request #${pr.number}`);
    }

    const trigger = pullRequestTriggerProblem(git(root, ["show", `${pushed}:${CI_WORKFLOW_PATH}`]));
    if (trigger !== undefined) {
      return no(`the pushed ${CI_WORKFLOW_PATH} ${trigger}, so not every event of the pull request is known to have run it`);
    }

    const runs = (await github.pullRequestRuns(head)).filter(
      (run) =>
        (run.path === CI_WORKFLOW_PATH || run.path.startsWith(`${CI_WORKFLOW_PATH}@`)) &&
        run.event === "pull_request" &&
        run.headSha === head,
    );
    if (runs.length === 0) {
      return no(`no pull_request run of ci.yml has the head ${short(head)} of pull request #${pr.number}`);
    }
    const unfinished = runs.find((run) => run.status !== "completed" || run.conclusion !== "success");
    if (unfinished !== undefined) {
      const state = unfinished.status !== "completed" ? unfinished.status || "not finished" : unfinished.conclusion || "no conclusion";
      return no(`run ${unfinished.id} of the head ${short(head)} of pull request #${pr.number} is ${state}, not success`);
    }

    const passed: Job[] = [];
    const notPassed: Partial<Record<Job, string>> = {};
    const jobsByRun = await Promise.all(runs.map(async (run) => ({ run, jobs: await github.jobsOf(run.id) })));
    for (const job of jobs) {
      const problem = jobsByRun
        .map(({ run, jobs: runJobs }) => {
          const reason = jobProblem(job, runJobs);
          return reason === undefined ? undefined : `${reason} in run ${run.id}`;
        })
        .find((reason) => reason !== undefined);
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
      baseRef: text(base.ref),
      baseRepository: text(baseRepo.full_name),
    };
  });
}

/** Reads the answer of GET /repos/{repo}/actions/runs/{id}/jobs. */
export function parseJobs(body: unknown): WorkflowJob[] {
  if (!isMap(body) || !Array.isArray(body.jobs)) {
    throw new Error("the GitHub API answered without a jobs list");
  }
  if (typeof body.total_count === "number" && body.total_count > body.jobs.length) {
    throw new Error(`the run has ${body.total_count} jobs, more than one page`);
  }
  return body.jobs.map((job: unknown, index): WorkflowJob => {
    if (!isMap(job) || typeof job.name !== "string") {
      throw new Error(`job ${index} in the GitHub API answer lacks a name`);
    }
    return { name: job.name, status: text(job.status), conclusion: text(job.conclusion) };
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

/** The merge base in the answer of GET /repos/{repo}/compare/{base}...{head}. */
export function parseMergeBase(body: unknown): string {
  const mergeBase = isMap(body) && isMap(body.merge_base_commit) ? text(body.merge_base_commit.sha) : "";
  if (!COMMIT_SHA.test(mergeBase)) {
    throw new Error("the comparison has no merge base commit");
  }
  return mergeBase;
}

/** The tree in the answer of GET /repos/{repo}/git/commits/{commit}, checked to be that commit's. */
export function parseCommitTree(body: unknown, commit: string): string {
  const tree = isMap(body) && isMap(body.tree) ? text(body.tree.sha) : "";
  if (!isMap(body) || body.sha !== commit || !COMMIT_SHA.test(tree)) {
    throw new Error(`the commit ${commit} came back without its tree`);
  }
  return tree;
}
