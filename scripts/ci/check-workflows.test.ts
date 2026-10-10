import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import {
  annotation,
  checkAction,
  checkRepository,
  checkWorkflow,
  findPolicyFiles,
  imageProblem,
  usesProblem,
  type Violation,
} from "./check-workflows.ts";

const SHA = "3d3c42e5aac5ba805825da76410c181273ba90b1";
const DIGEST = `sha256:${"0123456789abcdef".repeat(4)}`;
const SCRIPT = join(import.meta.dir, "check-workflows.ts");
const REPOSITORY_ROOT = join(import.meta.dir, "..", "..");

/** A workflow that follows every rule; tests change one thing at a time. */
function workflow(job: string, on = "on:\n  pull_request:\n"): string {
  return `${on}permissions: {}\njobs:\n  build:\n${job}`;
}

const PINNED_JOB = `    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@${SHA} # v7.0.1
      - uses: ./.github/actions/setup-proto
      - run: echo ok
`;

function paths(violations: Violation[]): string[] {
  return violations.map((violation) => violation.path);
}

describe("checkWorkflow", () => {
  test("accepts explicit permissions, SHA-pinned actions and local actions", () => {
    expect(checkWorkflow("ci.yml", workflow(PINNED_JOB))).toEqual([]);
  });

  test("rejects a job without permissions", () => {
    const job = PINNED_JOB.replace("    permissions:\n      contents: read\n", "");
    const violations = checkWorkflow("ci.yml", workflow(job));
    expect(violations).toEqual([
      {
        file: "ci.yml",
        path: "jobs.build",
        message: expect.stringContaining("missing `permissions`"),
      },
    ]);
  });

  test("rejects an action pinned to a tag instead of a commit SHA", () => {
    const job = PINNED_JOB.replace(`actions/checkout@${SHA} # v7.0.1`, "actions/checkout@v7");
    const violations = checkWorkflow("ci.yml", workflow(job));
    expect(violations).toEqual([
      {
        file: "ci.yml",
        path: "jobs.build.steps[0].uses",
        message: expect.stringContaining(
          "`actions/checkout@v7` is not pinned to a full 40-character commit SHA",
        ),
      },
    ]);
  });

  test("rejects permission shorthands and unknown levels", () => {
    const scopes = "permissions:\n      contents: read";
    const shorthand = PINNED_JOB.replace(scopes, "permissions: write-all");
    expect(paths(checkWorkflow("ci.yml", workflow(shorthand)))).toEqual([
      "jobs.build.permissions",
    ]);

    const level = PINNED_JOB.replace("contents: read", "contents: admin");
    expect(paths(checkWorkflow("ci.yml", workflow(level)))).toEqual([
      "jobs.build.permissions.contents",
    ]);

    const empty = PINNED_JOB.replace(scopes, "permissions:");
    expect(paths(checkWorkflow("ci.yml", workflow(empty)))).toEqual(["jobs.build.permissions"]);

    const topLevel = workflow(PINNED_JOB).replace("permissions: {}", "permissions: read-all");
    expect(paths(checkWorkflow("ci.yml", topLevel))).toEqual(["permissions"]);
  });

  test("accepts an empty permissions map", () => {
    const none = PINNED_JOB.replace("permissions:\n      contents: read", "permissions: {}");
    expect(checkWorkflow("ci.yml", workflow(none))).toEqual([]);
  });

  test("rejects an unpinned reusable workflow", () => {
    const job = `    permissions:
      contents: read
    uses: Laysi/workflows/.github/workflows/release.yml@main
`;
    expect(paths(checkWorkflow("ci.yml", workflow(job)))).toEqual(["jobs.build.uses"]);
    const pinned = job.replace("@main", `@${SHA}`);
    expect(checkWorkflow("ci.yml", workflow(pinned))).toEqual([]);
  });

  test("rejects job and service containers that are not pinned to a digest", () => {
    const job = `${PINNED_JOB}    container: rust:1.99
    services:
      postgres:
        image: postgres:18
      mosquitto:
        image: eclipse-mosquitto:2@${DIGEST}
`;
    expect(paths(checkWorkflow("ci.yml", workflow(job)))).toEqual([
      "jobs.build.container",
      "jobs.build.services.postgres.image",
    ]);
    const pinned = job
      .replace("rust:1.99", `rust:1.99@${DIGEST}`)
      .replace("postgres:18", `postgres:18@${DIGEST}`);
    expect(checkWorkflow("ci.yml", workflow(pinned))).toEqual([]);
  });

  test("rejects triggers that hand secrets to fork code, in every syntax", () => {
    for (const on of [
      "on:\n  pull_request_target:\n",
      "on: [push, pull_request_target]\n",
      "on: workflow_run\n",
    ]) {
      const violations = checkWorkflow("ci.yml", workflow(PINNED_JOB, on));
      expect(violations).toHaveLength(1);
      expect(violations[0]?.path).toMatch(/^on\.(pull_request_target|workflow_run)$/);
    }
  });

  test("reports every violation in one pass", () => {
    const job = `    steps:
      - uses: actions/checkout@v7
      - uses: Swatinem/rust-cache@${SHA.slice(0, 7)}
`;
    expect(paths(checkWorkflow("ci.yml", workflow(job)))).toEqual([
      "jobs.build",
      "jobs.build.steps[0].uses",
      "jobs.build.steps[1].uses",
    ]);
  });

  test("reports unreadable or malformed files instead of crashing", () => {
    expect(paths(checkWorkflow("ci.yml", "jobs: [\n"))).toEqual([""]);
    expect(paths(checkWorkflow("ci.yml", "- a list\n"))).toEqual([""]);
    expect(paths(checkWorkflow("ci.yml", "on: push\njobs: {}\n"))).toEqual(["jobs"]);
    expect(paths(checkWorkflow("ci.yml", "jobs:\n  a: 1\n"))).toEqual(["on", "jobs.a"]);
  });
});

describe("usesProblem", () => {
  test("accepts local actions, full SHAs and digest-pinned images", () => {
    expect(usesProblem("./.github/actions/setup-proto")).toBeUndefined();
    expect(usesProblem(`actions/checkout@${SHA}`)).toBeUndefined();
    expect(usesProblem(`owner/repo/path/to/action@${SHA}`)).toBeUndefined();
    expect(usesProblem(`docker://alpine:3@${DIGEST}`)).toBeUndefined();
  });

  test("rejects every other reference", () => {
    for (const reference of [
      "actions/checkout@v7",
      "actions/checkout@main",
      "actions/checkout",
      `actions/checkout@${SHA.slice(0, 12)}`,
      `actions/checkout@${SHA.toUpperCase()}`,
      `actions/checkout@${SHA}0`,
      `checkout@${SHA}`,
      "docker://alpine:3",
      "",
      42,
    ]) {
      expect(usesProblem(reference)).toBeString();
    }
  });
});

describe("imageProblem", () => {
  test("requires a digest", () => {
    expect(imageProblem(`postgres:18@${DIGEST}`)).toBeUndefined();
    expect(imageProblem("postgres:18")).toBeString();
    expect(imageProblem("${{ matrix.image }}")).toBeString();
    expect(imageProblem(undefined)).toBeString();
  });
});

describe("checkAction", () => {
  test("checks the steps of a composite action", () => {
    const action = `name: example
runs:
  using: composite
  steps:
    - uses: actions/checkout@v7
    - uses: actions/checkout@${SHA}
    - run: echo ok
      shell: bash
`;
    expect(paths(checkAction("action.yml", action))).toEqual(["runs.steps[0].uses"]);
  });

  test("checks the image of a Docker action", () => {
    const action = "runs:\n  using: docker\n  image: docker://alpine:3\n";
    expect(paths(checkAction("action.yml", action))).toEqual(["runs.image"]);
    expect(checkAction("action.yml", "runs:\n  using: docker\n  image: Dockerfile\n")).toEqual([]);
    expect(paths(checkAction("action.yml", "name: no runs\n"))).toEqual(["runs"]);
  });
});

describe("annotation", () => {
  test("escapes GitHub workflow command syntax", () => {
    expect(annotation({ file: "a,b:c.yml", path: "jobs.x", message: "50% done\nnext" })).toBe(
      "::error file=a%2Cb%3Ac.yml,title=workflow policy::jobs.x: 50%25 done%0Anext",
    );
  });
});

describe("repository scan and command line", () => {
  let root = "";

  function write(relativePath: string, contents: string): void {
    const path = join(root, relativePath);
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, contents);
  }

  afterAll(() => {
    if (root !== "") {
      rmSync(root, { recursive: true, force: true });
    }
  });

  beforeAll(() => {
    root = mkdtempSync(join(tmpdir(), "check-workflows-"));
    write(".github/workflows/good.yml", workflow(PINNED_JOB));
    write(
      ".github/workflows/bad.yaml",
      workflow(
        PINNED_JOB.replace("    permissions:\n      contents: read\n", "").replace(
          `actions/checkout@${SHA} # v7.0.1`,
          "actions/checkout@v7",
        ),
      ),
    );
    write(".github/workflows/nested/ignored.yml", "not: [valid\n");
    write(".github/workflows/README.md", "not a workflow\n");
    write(
      ".github/actions/setup/action.yml",
      "runs:\n  using: composite\n  steps:\n    - uses: actions/cache@v4\n",
    );
  });

  test("finds top-level workflows and every local action", () => {
    const files = findPolicyFiles(root);
    expect(files.workflows.map((path) => path.slice(root.length + 1))).toEqual([
      ".github/workflows/bad.yaml",
      ".github/workflows/good.yml",
    ]);
    expect(files.actions.map((path) => path.slice(root.length + 1))).toEqual([
      ".github/actions/setup/action.yml",
    ]);
  });

  test("labels violations with the file relative to the root", () => {
    const { files, violations } = checkRepository(root);
    expect(files).toBe(3);
    expect(violations.map((violation) => `${violation.file} ${violation.path}`)).toEqual([
      ".github/workflows/bad.yaml jobs.build",
      ".github/workflows/bad.yaml jobs.build.steps[0].uses",
      ".github/actions/setup/action.yml runs.steps[0].uses",
    ]);
  });

  test("the command fails on an unpinned action and a job without permissions", () => {
    const result = Bun.spawnSync([process.execPath, SCRIPT, root], {
      env: { ...process.env, GITHUB_ACTIONS: "true" },
    });
    const stdout = result.stdout.toString();
    expect(result.exitCode).toBe(1);
    expect(stdout).toContain(".github/workflows/bad.yaml: jobs.build: missing `permissions`");
    expect(stdout).toContain(
      "`actions/checkout@v7` is not pinned to a full 40-character commit SHA",
    );
    expect(stdout).toContain("::error file=.github/workflows/bad.yaml,title=workflow policy::");
    expect(stdout).toContain("workflow policy: 3 violation(s) in 3 file(s)");
  });

  test("the command fails when there is nothing to check", () => {
    const empty = mkdtempSync(join(tmpdir(), "check-workflows-empty-"));
    try {
      const result = Bun.spawnSync([process.execPath, SCRIPT, empty]);
      expect(result.exitCode).toBe(2);
      expect(result.stderr.toString()).toContain("no workflow or action files");
    } finally {
      rmSync(empty, { recursive: true, force: true });
    }
  });

  test("this repository's workflows and actions pass", () => {
    const result = Bun.spawnSync([process.execPath, SCRIPT, REPOSITORY_ROOT]);
    expect(result.stdout.toString()).toContain("no violations");
    expect(result.exitCode).toBe(0);
  });
});
