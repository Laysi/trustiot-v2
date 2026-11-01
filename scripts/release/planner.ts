/**
 * The planning logic behind plan.ts, separate from the command line so that plan.test.ts can run
 * it against throwaway git repositories.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { targetStageRunProblems } from "./containerfile.ts";
import { run, systemTool, tryRun } from "./exec.ts";
import {
  CHART_DIR,
  type ImageSpec,
  type ReleasePlan,
  type ReleaseVersion,
  ReleaseError,
  type WorkflowRun,
  discoverImages,
  greenCiProblem,
  isMap,
  parsePlan,
  parseReleaseTag,
  planImage,
  previousReleaseTag,
} from "./lib.ts";

export interface PlanInput {
  /** The repository root (a git work tree). */
  root: string;
  tag: string;
  /** Local build of HEAD: the tag need not exist and the main and ci checks are skipped. */
  dryRun: boolean;
  /** Registry path the images and chart go to. */
  registry: string;
  /** `owner/name` as GitHub spells it. */
  repository: string;
  /**
   * Lists the push runs of ci.yml on main for a commit (plan.ts asks the GitHub API). A final
   * release needs one; release candidates and dry runs never call it.
   */
  ciRuns?: (commit: string) => Promise<WorkflowRun[]>;
}

async function git(root: string, ...args: string[]): Promise<string> {
  return (await run([systemTool("git"), "-C", root, ...args])).stdout.trim();
}

/** Throws unless `commit` is reachable from origin/main. */
async function requireOnMain(root: string, commit: string): Promise<void> {
  const main = "refs/remotes/origin/main";
  const exists = await tryRun([systemTool("git"), "-C", root, "rev-parse", "--verify", "--quiet", main]);
  if (exists.exitCode !== 0) {
    throw new ReleaseError(`${main} is missing; fetch main before planning a final release`);
  }
  const ancestor = await tryRun([systemTool("git"), "-C", root, "merge-base", "--is-ancestor", commit, main]);
  if (ancestor.exitCode === 1) {
    throw new ReleaseError(
      `commit ${commit} is not on main: a final release tag must point at a commit on main; ` +
        "tag a release candidate (vX.Y.Z-rc.N) to release from another branch",
    );
  }
  if (ancestor.exitCode !== 0) {
    throw new ReleaseError(`git merge-base failed: ${ancestor.stderr.trim()}`);
  }
}

/** Throws unless the newest push run of ci.yml on main for `commit` succeeded. */
async function requireGreenCi(input: PlanInput, commit: string): Promise<void> {
  if (input.ciRuns === undefined) {
    throw new ReleaseError("a final release needs the ci runs of the tagged commit on main, and nothing can read them here");
  }
  const problem = greenCiProblem(await input.ciRuns(commit), commit);
  if (problem !== undefined) {
    throw new ReleaseError(problem);
  }
}

/** The chart name from Chart.yaml. */
function chartName(root: string): string {
  const chart = Bun.YAML.parse(readFileSync(join(root, CHART_DIR, "Chart.yaml"), "utf8"));
  if (!isMap(chart) || typeof chart.name !== "string" || chart.name === "") {
    throw new ReleaseError(`${CHART_DIR}/Chart.yaml has no name`);
  }
  return chart.name;
}

/**
 * Checks the tag against the checked-out commit and lists what the release publishes. A final
 * release (no -rc.N) must be on main, and ci must have passed for that commit on main.
 */
export async function makePlan(input: PlanInput): Promise<ReleasePlan> {
  const version = parseReleaseTag(input.tag);
  const commit = await git(input.root, "rev-parse", "HEAD");
  if (!input.dryRun) {
    const tagged = await tryRun([systemTool("git"), "-C", input.root, "rev-parse", "--verify", "--quiet", `refs/tags/${version.tag}^{commit}`]);
    if (tagged.exitCode !== 0) {
      throw new ReleaseError(`tag ${version.tag} does not exist in this checkout`);
    }
    if (tagged.stdout.trim() !== commit) {
      throw new ReleaseError(`tag ${version.tag} points at ${tagged.stdout.trim()}, but ${commit} is checked out`);
    }
    if (!version.prerelease) {
      await requireOnMain(input.root, commit);
      await requireGreenCi(input, commit);
    }
  }
  const sourceDateEpoch = Number(await git(input.root, "log", "-1", "--format=%ct", commit));
  const name = chartName(input.root);
  const plan: ReleasePlan = {
    ...version,
    commit,
    sourceDateEpoch,
    repository: input.repository,
    images: checkContainerfiles(input.root, discoverImages(input.root)).map((spec) => planImage(spec, version.version, input.registry)),
    chart: { name, repository: `${input.registry}/charts/${name}`, version: version.version },
  };
  // The other stages read the plan through this validator; fail here if it would not pass.
  return parsePlan(JSON.stringify(plan));
}

/**
 * Returns the images unchanged, or throws when a Containerfile, of a Containerfile image or the
 * own Containerfile of a Rust image, runs a program in a stage that builds for the image's
 * platform (containerfile.ts targetStageRunProblems).
 */
function checkContainerfiles(root: string, images: ImageSpec[]): ImageSpec[] {
  for (const image of images) {
    const where = image.kind === "containerfile" ? `${image.context}/Containerfile` : image.containerfile;
    if (where === undefined) {
      continue;
    }
    const problems = targetStageRunProblems(readFileSync(join(root, where), "utf8"));
    if (problems.length > 0) {
      throw new ReleaseError(`${where}: ${problems.join("; ")}`);
    }
  }
  return images;
}

/** The release whose tag the notes of `release` start from (lib.ts previousReleaseTag). */
export async function previousTag(root: string, release: ReleaseVersion): Promise<string | undefined> {
  const tags = (await git(root, "tag", "--list", "v*")).split("\n").filter((tag) => tag !== "");
  return previousReleaseTag(tags, release);
}
