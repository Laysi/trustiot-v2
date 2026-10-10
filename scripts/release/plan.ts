/**
 * Release stage 1, the `plan` job of .github/workflows/release.yml: checks the tag and lists
 * what the release publishes. Reads only the repository and git; runs no build code.
 *
 *   bun scripts/release/plan.ts --tag <vX.Y.Z or vX.Y.Z-rc.N> [--output <plan.json>] [--dry-run]
 *
 * - The tag must be a release tag (lib.ts parseReleaseTag) and point at the checked-out commit.
 *   A final release (no -rc.N) must be on main (refs/remotes/origin/main), and the newest push
 *   run of ci.yml on main for that commit must have succeeded (lib.ts greenCiProblem). The runs
 *   are read from the GitHub API with GITHUB_TOKEN, which needs `actions: read`.
 * - The images are the directories under deploy/images/ (lib.ts discoverImages).
 * - The chart must pass `helm lint --strict` for both deployment modes, as in CI.
 *
 * --dry-run plans a local build of HEAD: the tag need not exist and the main and ci checks are
 * skipped; with it, --registry <host/path> points the plan at a local registry instead of
 * ghcr.io/laysi/trustiot-v2 (see registry.ts). The workflow passes neither. The plan is written
 * to --output and, in GitHub Actions, to the step outputs `plan`, `version`, `prerelease` and
 * `previous-tag` (where the release notes start; empty for the first release).
 */

import { writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { log, parseOptions, protoTool, run, runStage, setOutput } from "./exec.ts";
import { CHART_DIR, REGISTRY, ReleaseError, type WorkflowRun, ciRunsUrl, parseWorkflowRuns } from "./lib.ts";
import { makePlan, previousTag } from "./planner.ts";

const ROOT = resolve(import.meta.dir, "..", "..");
/** The repository as GitHub spells it, when GITHUB_REPOSITORY is not set (local dry runs). */
const DEFAULT_REPOSITORY = "Laysi/trustiot-v2";

/** The push runs of ci.yml on main for `commit`, from the GitHub API. */
async function ciRunsOnMain(repository: string, commit: string): Promise<WorkflowRun[]> {
  const token = process.env.GITHUB_TOKEN;
  if (!token) {
    throw new ReleaseError("GITHUB_TOKEN (with actions: read) must be set to check ci on main for a final release");
  }
  const url = ciRunsUrl(process.env.GITHUB_API_URL || "https://api.github.com", repository, commit);
  const response = await fetch(url, {
    headers: {
      Accept: "application/vnd.github+json",
      Authorization: `Bearer ${token}`,
      "X-GitHub-Api-Version": "2022-11-28",
    },
    signal: AbortSignal.timeout(30_000),
  });
  if (!response.ok) {
    throw new ReleaseError(
      `listing the ci runs on main failed with HTTP ${response.status}` +
        (response.status === 403 || response.status === 404 ? " (the plan job needs `actions: read`)" : ""),
    );
  }
  const runs = parseWorkflowRuns(await response.json());
  log("plan", `${runs.length} ci run(s) on main for ${commit}`);
  return runs;
}

/** `helm lint --strict` with each deployment mode and its test install values, as in CI. */
async function lintChart(): Promise<void> {
  const helm = await protoTool("helm");
  for (const mode of ["saas", "onprem"]) {
    await run(
      [helm, "lint", CHART_DIR, "--strict", "-f", `${CHART_DIR}/values-${mode}.yaml`, "-f", `${CHART_DIR}/tests/values/${mode}-instance.yaml`],
      { cwd: ROOT },
    );
    log("plan", `helm lint --strict passed with values-${mode}.yaml`);
  }
}

await runStage("plan", async () => {
  const options = parseOptions(Bun.argv.slice(2), {
    tag: {},
    output: { default: "" },
    registry: { default: REGISTRY },
    "dry-run": { flag: true },
  });
  const dryRun = options["dry-run"] === "true";
  const registry = options.registry ?? REGISTRY;
  if (registry !== REGISTRY && (!dryRun || process.env.GITHUB_ACTIONS === "true")) {
    throw new ReleaseError(`--registry is only for local dry runs; releases publish to ${REGISTRY}`);
  }
  const repository = process.env.GITHUB_REPOSITORY || DEFAULT_REPOSITORY;
  const plan = await makePlan({
    root: ROOT,
    tag: options.tag ?? "",
    dryRun,
    registry,
    repository,
    ciRuns: (commit) => ciRunsOnMain(repository, commit),
  });
  await lintChart();
  const previous = await previousTag(ROOT, plan);

  log("plan", `${plan.tag} → version ${plan.version}${plan.prerelease ? " (prerelease)" : ""}, commit ${plan.commit}`);
  for (const image of plan.images) {
    log("plan", `image ${image.ref} (${image.kind === "rust" ? `Rust package ${image.package}` : image.context})`);
  }
  log("plan", `chart ${plan.chart.repository}:${plan.chart.version}`);
  log("plan", previous === undefined ? "first release: the notes list every commit" : `notes start from ${previous}`);
  if (options.output) {
    writeFileSync(options.output, `${JSON.stringify(plan, null, 2)}\n`);
  }
  setOutput("plan", JSON.stringify(plan));
  setOutput("version", plan.version);
  setOutput("prerelease", String(plan.prerelease));
  setOutput("previous-tag", previous ?? "");
});
