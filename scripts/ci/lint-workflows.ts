/**
 * Syntax and expression check for GitHub Actions workflows, using actionlint
 * (docs/architecture/ci-release.md).
 *
 * `check-workflows.ts` enforces this repository's own rules (permissions, pinning, triggers)
 * and reads the YAML only as data. It cannot tell whether GitHub accepts a file. actionlint
 * can: it knows which contexts and functions are available at each key, so it catches what
 * GitHub otherwise reports only when it parses the file ("This run likely failed because of a
 * workflow file issue"), for example `${{ runner.temp }}` in `jobs.<id>.env`.
 *
 * It checks every `.github/workflows/*.yml` (or `.yaml`) file. actionlint does not read an
 * `action.yml` as a file of its own: a composite action under `.github/actions/` is covered
 * through the workflows that use it (inputs given, required inputs missing, outputs read).
 * Its steps are covered by `check-workflows.ts` only.
 *
 * The shellcheck and pyflakes integrations are switched off on purpose. actionlint uses them
 * only when they are on PATH, so a runner image that happens to ship shellcheck would check
 * more than a laptop that does not: the result must not depend on the machine. See
 * "Workflow 語法檢查" in docs/architecture/ci-release.md.
 *
 * The binary is the version pinned in `.prototools`, found with `proto bin actionlint`
 * (`proto install actionlint` installs it).
 *
 * Usage: bun scripts/ci/lint-workflows.ts [repository-root]
 * Exit status: 0 when actionlint reports nothing, 1 when it reports an error, 2 when it
 * cannot run or there is nothing to check.
 */

import { existsSync } from "node:fs";
import { join, relative, resolve } from "node:path";

import { findPolicyFiles } from "./check-workflows.ts";

/** The repository this script lives in; its `.prototools` pins actionlint. */
const TOOLCHAIN_ROOT = resolve(import.meta.dir, "..", "..");

/** Flags that make the result independent of the tools installed on the machine. */
export const ACTIONLINT_FLAGS: readonly string[] = ["-shellcheck=", "-pyflakes=", "-no-color"];

/** The path of the pinned actionlint binary, or an error message that says how to get it. */
export function locateActionlint(): { path: string } | { error: string } {
  let result: Bun.SyncSubprocess<"pipe", "pipe">;
  try {
    result = Bun.spawnSync(["proto", "bin", "actionlint", "--reporter", "text"], {
      cwd: TOOLCHAIN_ROOT,
    });
  } catch (cause) {
    return { error: `cannot run proto (${String(cause)}); install proto and run \`proto install\`` };
  }
  const path = result.stdout.toString().trim();
  if (result.exitCode !== 0 || path === "") {
    return {
      error: `actionlint is not installed at the version pinned in .prototools; run \`proto install actionlint\`\n${result.stderr.toString().trim()}`,
    };
  }
  return { path };
}

/** Run actionlint on the given files (relative to `root`); returns its exit status and output. */
export function runActionlint(
  binary: string,
  root: string,
  files: readonly string[],
): { exitCode: number; output: string } {
  const result = Bun.spawnSync([binary, ...ACTIONLINT_FLAGS, ...files], { cwd: root });
  return {
    exitCode: result.exitCode ?? 2,
    output: `${result.stdout.toString()}${result.stderr.toString()}`,
  };
}

export function main(args: readonly string[]): number {
  const root = resolve(args[0] ?? join(import.meta.dir, "..", ".."));
  const workflows = findPolicyFiles(root).workflows.map((file) => relative(root, file));
  if (workflows.length === 0) {
    console.error(`lint-workflows: no workflow files under ${root}/.github/workflows`);
    return 2;
  }

  // actionlint finds the project root by looking for `.git` (a directory, or the file a git
  // worktree has). Without it, it silently skips checking local actions' inputs.
  if (!existsSync(join(root, ".git"))) {
    console.error(
      `lint-workflows: ${root} has no .git; actionlint needs it to check local actions, run this in a git checkout`,
    );
    return 2;
  }

  const located = locateActionlint();
  if ("error" in located) {
    console.error(`lint-workflows: ${located.error}`);
    return 2;
  }

  const { exitCode, output } = runActionlint(located.path, root, workflows);
  if (output.trim() !== "") {
    console.log(output.trimEnd());
  }
  // actionlint exits 1 for lint errors; anything else (2 and up) means it could not run.
  if (exitCode === 0) {
    console.log(`actionlint: ${workflows.length} workflow file(s), no errors`);
    return 0;
  }
  if (exitCode === 1) {
    console.log(`actionlint: errors found (${workflows.length} workflow file(s) checked)`);
    return 1;
  }
  console.error(`lint-workflows: actionlint failed to run (exit status ${exitCode})`);
  return 2;
}

if (import.meta.main) {
  process.exit(main(Bun.argv.slice(2)));
}
