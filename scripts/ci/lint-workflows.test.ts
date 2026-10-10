import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { ACTIONLINT_FLAGS, locateActionlint } from "./lint-workflows.ts";

const SCRIPT = join(import.meta.dir, "lint-workflows.ts");
const REPOSITORY_ROOT = join(import.meta.dir, "..", "..");

const HEADER = "on:\n  pull_request:\npermissions: {}\n";
const STEP = "      - run: echo ok\n";

/** A job with the given extra lines (already indented by four spaces) before `steps`. */
function job(extra = "", steps = STEP): string {
  return `    runs-on: ubuntu-24.04\n${extra}    steps:\n${steps}`;
}

function lint(root: string): { exitCode: number; stdout: string; stderr: string } {
  const result = Bun.spawnSync([process.execPath, SCRIPT, root]);
  return {
    exitCode: result.exitCode ?? -1,
    stdout: result.stdout.toString(),
    stderr: result.stderr.toString(),
  };
}

let scratch: string;
let counter = 0;

/**
 * A repository root holding the given files (path relative to the root -> content). It has
 * an empty `.git` directory, which is how actionlint recognises a project root.
 */
function repository(files: Record<string, string>, git = true): string {
  const root = join(scratch, `repo-${counter++}`);
  if (git) {
    mkdirSync(join(root, ".git"), { recursive: true });
  }
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(root, path)), { recursive: true });
    writeFileSync(join(root, path), content);
  }
  return root;
}

beforeAll(() => {
  scratch = mkdtempSync(join(tmpdir(), "lint-workflows-"));
});

afterAll(() => {
  rmSync(scratch, { recursive: true, force: true });
});

describe("actionlint binary", () => {
  test("is the version pinned in .prototools", () => {
    const located = locateActionlint();
    if ("error" in located) {
      throw new Error(located.error);
    }
    const result = Bun.spawnSync([located.path, "-version"]);
    const pinned = /^actionlint = "([^"]+)"$/m.exec(
      readFileSync(join(REPOSITORY_ROOT, ".prototools"), "utf8"),
    );
    expect(pinned).not.toBeNull();
    expect(result.stdout.toString().split("\n")[0]).toBe(pinned?.[1] ?? "");
  });

  test("runs without the optional shellcheck and pyflakes integrations", () => {
    expect(ACTIONLINT_FLAGS).toContain("-shellcheck=");
    expect(ACTIONLINT_FLAGS).toContain("-pyflakes=");
  });
});

describe("lint-workflows", () => {
  test("rejects the runner context in job-level env, which GitHub cannot parse", () => {
    const root = repository({
      ".github/workflows/nightly.yml": `name: nightly\n${HEADER}jobs:\n  smoke:\n${job(
        "    env:\n      WORK_DIR: ${{ runner.temp }}/smoke\n",
      )}`,
    });
    const result = lint(root);
    expect(result.exitCode).toBe(1);
    expect(result.stdout).toContain(".github/workflows/nightly.yml:");
    expect(result.stdout).toContain('context "runner" is not allowed here');
  });

  test("accepts the same value in a step-level env, where GitHub allows it", () => {
    const root = repository({
      ".github/workflows/nightly.yml": `name: nightly\n${HEADER}jobs:\n  smoke:\n${job(
        "",
        "      - run: echo \"$WORK_DIR\"\n        env:\n          WORK_DIR: ${{ runner.temp }}/smoke\n",
      )}`,
    });
    const result = lint(root);
    expect(result.stdout).toContain("no errors");
    expect(result.exitCode).toBe(0);
  });

  test("rejects an unknown key and a malformed expression", () => {
    const root = repository({
      ".github/workflows/typo.yaml": `name: typo\n${HEADER}jobs:\n  a:\n${job("    timeout-minute: 5\n")}`,
      ".github/workflows/expression.yml": `name: expression\n${HEADER}jobs:\n  a:\n${job(
        "",
        "      - run: echo ok\n        if: ${{ github.event_name == }}\n",
      )}`,
    });
    const result = lint(root);
    expect(result.exitCode).toBe(1);
    expect(result.stdout).toContain(".github/workflows/typo.yaml:");
    expect(result.stdout).toContain(".github/workflows/expression.yml:");
  });

  test("checks the inputs a workflow gives a local composite action", () => {
    const root = repository({
      ".github/actions/setup/action.yml": `name: setup
description: Example.
inputs:
  tools:
    description: Tools.
    required: true
runs:
  using: composite
  steps:
    - run: echo "$TOOLS"
      shell: bash
      env:
        TOOLS: \${{ inputs.tools }}
`,
      ".github/workflows/ok.yml": `name: ok\n${HEADER}jobs:\n  a:\n${job(
        "",
        "      - uses: ./.github/actions/setup\n        with:\n          tools: bun\n",
      )}`,
      ".github/workflows/missing-input.yml": `name: missing\n${HEADER}jobs:\n  a:\n${job(
        "",
        "      - uses: ./.github/actions/setup\n",
      )}`,
    });
    const result = lint(root);
    expect(result.exitCode).toBe(1);
    expect(result.stdout).toContain(".github/workflows/missing-input.yml:");
    expect(result.stdout).toContain('missing input "tools"');
    expect(result.stdout).not.toContain(".github/workflows/ok.yml:");
  });

  test("does not depend on shellcheck being installed", () => {
    // SC2086 (unquoted variable) is what shellcheck would report here; the result must be the
    // same on a machine that has shellcheck and one that does not.
    const root = repository({
      ".github/workflows/shell.yml": `name: shell\n${HEADER}jobs:\n  a:\n${job(
        "",
        "      - run: echo $HOME\n",
      )}`,
    });
    const result = lint(root);
    expect(result.stdout).toContain("no errors");
    expect(result.exitCode).toBe(0);
  });

  test("fails when there is nothing to check", () => {
    const result = lint(repository({ "README.md": "empty\n" }));
    expect(result.exitCode).toBe(2);
    expect(result.stderr).toContain("no workflow files");
  });

  test("fails rather than skip local action checks when the root is not a git checkout", () => {
    const root = repository(
      { ".github/workflows/ok.yml": `name: ok\n${HEADER}jobs:\n  a:\n${job()}` },
      false,
    );
    const result = lint(root);
    expect(result.exitCode).toBe(2);
    expect(result.stderr).toContain("has no .git");
  });

  test("this repository's workflows pass", () => {
    const result = lint(REPOSITORY_ROOT);
    expect(result.stdout).toContain("no errors");
    expect(result.exitCode).toBe(0);
  });
});
