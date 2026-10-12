import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CI_WORKFLOW_PATH, REGISTRY, type WorkflowRun, parseReleaseTag } from "./lib.ts";
import { makePlan, previousTag } from "./planner.ts";

// Throwaway repositories are built with git plumbing and without the user's or the system's git
// configuration, so no hook runs and nothing is signed.
const GIT_ENV = {
  ...process.env,
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_AUTHOR_NAME: "release test",
  GIT_AUTHOR_EMAIL: "release-test@example.invalid",
  GIT_COMMITTER_NAME: "release test",
  GIT_COMMITTER_EMAIL: "release-test@example.invalid",
};
const MAIN_EPOCH = 1_791_600_000;
const BRANCH_EPOCH = 1_791_630_000;

const roots: string[] = [];
afterAll(() => {
  for (const root of roots) {
    rmSync(root, { recursive: true, force: true });
  }
});

function git(root: string, args: string[], env: Record<string, string> = {}): string {
  const result = Bun.spawnSync(["git", "-C", root, ...args], { env: { ...GIT_ENV, ...env }, stderr: "pipe" });
  if (result.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")}: ${result.stderr.toString()}`);
  }
  return result.stdout.toString().trim();
}

function commit(root: string, epoch: number, parent?: string): string {
  git(root, ["add", "--all"]);
  const tree = git(root, ["write-tree"]);
  const date = `@${epoch} +0000`;
  return git(root, ["commit-tree", "--no-gpg-sign", tree, ...(parent ? ["-p", parent] : []), "-m", `commit at ${epoch}`], {
    GIT_AUTHOR_DATE: date,
    GIT_COMMITTER_DATE: date,
  });
}

interface Repo {
  root: string;
  /** The commit origin/main points at. */
  main: string;
  /** A commit on a branch, not on main. HEAD is detached at it, as actions/checkout leaves it. */
  branch: string;
}

/** A repository with a chart, a Rust image and a Containerfile image, and origin/main one commit behind HEAD. */
function repo(options: { originMain?: boolean } = {}): Repo {
  const root = mkdtempSync(join(tmpdir(), "release-plan-test-"));
  roots.push(root);
  git(root, ["init", "--quiet"]);
  mkdirSync(join(root, "deploy/chart"), { recursive: true });
  mkdirSync(join(root, "deploy/images/identity"), { recursive: true });
  mkdirSync(join(root, "deploy/images/postgres-age"), { recursive: true });
  writeFileSync(join(root, "deploy/chart/Chart.yaml"), "apiVersion: v2\nname: trustiot\nversion: 0.0.0\n");
  writeFileSync(join(root, "deploy/images/identity/image.toml"), '[rust]\npackage = "identity"\n');
  writeFileSync(join(root, "deploy/images/postgres-age/Containerfile"), "FROM scratch\n");
  const main = commit(root, MAIN_EPOCH);
  writeFileSync(join(root, "deploy/images/postgres-age/Containerfile"), "FROM scratch\nLABEL x=y\n");
  const branch = commit(root, BRANCH_EPOCH, main);
  if (options.originMain !== false) {
    git(root, ["update-ref", "refs/remotes/origin/main", main]);
  }
  git(root, ["update-ref", "--no-deref", "HEAD", branch]);
  return { root, main, branch };
}

type CiRuns = (commit: string) => Promise<WorkflowRun[]>;

/** A successful push run of ci.yml on main for `commit`. */
function greenRun(commit: string, overrides: Partial<WorkflowRun> = {}): WorkflowRun {
  return {
    id: 7,
    path: CI_WORKFLOW_PATH,
    headSha: commit,
    headBranch: "main",
    event: "push",
    status: "completed",
    conclusion: "success",
    url: "https://github.com/Laysi/trustiot-v2/actions/runs/7",
    ...overrides,
  };
}

/** GitHub as seen by the plan: the ci runs it lists for each commit; records what was asked. */
function fakeCi(runs: WorkflowRun[]): CiRuns & { asked: string[] } {
  const asked: string[] = [];
  const ciRuns = async (commit: string) => {
    asked.push(commit);
    return runs.filter((run) => run.headSha === commit);
  };
  return Object.assign(ciRuns, { asked });
}

/** A ci lookup that must not happen. */
const noCi: CiRuns = async (commit) => {
  throw new Error(`ci runs were asked for ${commit}`);
};

/** `ciRuns: null` plans without any way to read ci runs. */
function plan(root: string, tag: string, dryRun = false, ciRuns: CiRuns | null = noCi) {
  return makePlan({ root, tag, dryRun, registry: REGISTRY, repository: "Laysi/trustiot-v2", ciRuns: ciRuns ?? undefined });
}

describe("makePlan", () => {
  test("plans a release candidate tagged on a branch", async () => {
    const { root, branch } = repo();
    git(root, ["update-ref", "refs/tags/v0.3.0-rc.2", branch]);
    const result = await plan(root, "v0.3.0-rc.2");
    expect(result).toEqual({
      tag: "v0.3.0-rc.2",
      version: "0.3.0-rc.2",
      prerelease: true,
      commit: branch,
      sourceDateEpoch: BRANCH_EPOCH,
      repository: "Laysi/trustiot-v2",
      images: [
        {
          name: "identity",
          kind: "rust",
          package: "identity",
          repository: `${REGISTRY}/identity`,
          ref: `${REGISTRY}/identity:0.3.0-rc.2`,
        },
        {
          name: "postgres-age",
          kind: "containerfile",
          context: "deploy/images/postgres-age",
          repository: `${REGISTRY}/postgres-age`,
          ref: `${REGISTRY}/postgres-age:0.3.0-rc.2`,
        },
      ],
      chart: { name: "trustiot", repository: `${REGISTRY}/charts/trustiot`, version: "0.3.0-rc.2" },
    });
  });

  test("accepts an annotated tag", async () => {
    const { root, branch } = repo();
    git(root, ["tag", "--annotate", "--message", "release candidate", "v0.3.0-rc.3", branch]);
    expect(git(root, ["cat-file", "-t", "refs/tags/v0.3.0-rc.3"])).toBe("tag");
    expect((await plan(root, "v0.3.0-rc.3")).commit).toBe(branch);
  });

  test("refuses a final release that is not on main", async () => {
    const { root, branch } = repo();
    git(root, ["update-ref", "refs/tags/v0.3.0", branch]);
    await expect(plan(root, "v0.3.0")).rejects.toThrow("is not on main");
  });

  test("plans a final release on main once ci passed there", async () => {
    const { root, main } = repo();
    git(root, ["update-ref", "--no-deref", "HEAD", main]);
    git(root, ["update-ref", "refs/tags/v0.3.0", main]);
    const ci = fakeCi([greenRun(main)]);
    const result = await plan(root, "v0.3.0", false, ci);
    expect(result.prerelease).toBe(false);
    expect(result.commit).toBe(main);
    expect(result.sourceDateEpoch).toBe(MAIN_EPOCH);
    expect(ci.asked).toEqual([main]);
  });

  test("refuses a final release on main without a green ci run for its commit", async () => {
    const { root, main, branch } = repo();
    git(root, ["update-ref", "--no-deref", "HEAD", main]);
    git(root, ["update-ref", "refs/tags/v0.3.0", main]);
    // No run at all, or a green run only for another commit or for the pull request.
    for (const runs of [[], [greenRun(branch)], [greenRun(main, { event: "pull_request", headBranch: "feature" })]]) {
      await expect(plan(root, "v0.3.0", false, fakeCi(runs))).rejects.toThrow(`no ci run on main for ${main}`);
    }
    await expect(plan(root, "v0.3.0", false, fakeCi([greenRun(main, { status: "in_progress", conclusion: "" })]))).rejects.toThrow(
      "is in_progress",
    );
    await expect(plan(root, "v0.3.0", false, fakeCi([greenRun(main, { conclusion: "failure" })]))).rejects.toThrow(
      "ended with failure",
    );
  });

  test("refuses a final release when the ci runs cannot be read", async () => {
    const { root, main } = repo();
    git(root, ["update-ref", "--no-deref", "HEAD", main]);
    git(root, ["update-ref", "refs/tags/v0.3.0", main]);
    await expect(plan(root, "v0.3.0", false, null)).rejects.toThrow("needs the ci runs of the tagged commit");
    const failing: CiRuns = async () => {
      throw new Error("HTTP 403");
    };
    await expect(plan(root, "v0.3.0", false, failing)).rejects.toThrow("HTTP 403");
  });

  test("checks main before ci, and never asks for ci for a release candidate or a dry run", async () => {
    const { root, main, branch } = repo();
    git(root, ["update-ref", "refs/tags/v0.3.0", branch]);
    const ci = fakeCi([greenRun(branch)]);
    await expect(plan(root, "v0.3.0", false, ci)).rejects.toThrow("is not on main");
    expect(ci.asked).toEqual([]);
    git(root, ["update-ref", "refs/tags/v0.3.0-rc.1", branch]);
    expect((await plan(root, "v0.3.0-rc.1", false, noCi)).prerelease).toBe(true);
    expect((await plan(root, "v0.4.0", true, noCi)).commit).toBe(branch);
    git(root, ["update-ref", "--no-deref", "HEAD", main]);
    git(root, ["update-ref", "refs/tags/v0.3.0-rc.2", main]);
    expect((await plan(root, "v0.3.0-rc.2", false, noCi)).commit).toBe(main);
  });

  test("refuses a final release when origin/main was not fetched", async () => {
    const { root, main } = repo({ originMain: false });
    git(root, ["update-ref", "--no-deref", "HEAD", main]);
    git(root, ["update-ref", "refs/tags/v0.3.0", main]);
    await expect(plan(root, "v0.3.0")).rejects.toThrow("refs/remotes/origin/main is missing");
  });

  test("refuses a tag that points at another commit than the checkout", async () => {
    const { root, main } = repo();
    git(root, ["update-ref", "refs/tags/v0.3.0-rc.1", main]);
    await expect(plan(root, "v0.3.0-rc.1")).rejects.toThrow(`points at ${main}`);
  });

  test("refuses a tag that does not exist, unless it is a dry run", async () => {
    const { root, branch } = repo();
    await expect(plan(root, "v0.3.0-rc.1")).rejects.toThrow("does not exist");
    const result = await plan(root, "v0.3.0", true);
    expect(result.commit).toBe(branch);
    expect(result.prerelease).toBe(false);
  });

  test("refuses a tag that is not a release tag", async () => {
    const { root, branch } = repo();
    git(root, ["update-ref", "refs/tags/latest", branch]);
    await expect(plan(root, "latest")).rejects.toThrow("is not a release tag");
  });

  test("refuses a Containerfile that runs a program in the image's stage", async () => {
    // Laysi/trustiot-v2#310: on the arm64 runner such a RUN would run, but x86 machines could
    // not build the image without an emulator.
    const { root } = repo();
    const pinned = `docker.io/library/debian:13@sha256:${"d".repeat(64)}`;
    writeFileSync(join(root, "deploy/images/postgres-age/Containerfile"), `FROM ${pinned}\nRUN apt-get update\n`);
    await expect(plan(root, "v0.3.0-rc.1", true)).rejects.toThrow(
      "deploy/images/postgres-age/Containerfile: line 2: RUN in the stage of line 1, which builds for the image's platform",
    );
    writeFileSync(
      join(root, "deploy/images/postgres-age/Containerfile"),
      `FROM --platform=$BUILDPLATFORM ${pinned} AS build\nRUN make\nFROM ${pinned}\nCOPY --from=build /out /\n`,
    );
    expect((await plan(root, "v0.3.0-rc.1", true)).images.map((image) => image.name)).toEqual(["identity", "postgres-age"]);
  });
});

describe("previousTag", () => {
  test("reads the release tags of the checkout", async () => {
    const { root, main, branch } = repo();
    for (const [tag, commit] of [
      ["v0.2.0", main],
      ["v0.3.0-rc.1", main],
      ["v0.3.0-rc.2", branch],
      ["v0.2.0-hotfix", branch],
    ] as const) {
      git(root, ["update-ref", `refs/tags/${tag}`, commit]);
    }
    expect(await previousTag(root, parseReleaseTag("v0.3.0-rc.2"))).toBe("v0.3.0-rc.1");
    expect(await previousTag(root, parseReleaseTag("v0.3.0"))).toBe("v0.2.0");
    expect(await previousTag(root, parseReleaseTag("v0.1.0"))).toBeUndefined();
  });
});
