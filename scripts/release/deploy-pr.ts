/**
 * Release stage 5, the `deploy-pr` job of .github/workflows/release.yml: after verify passed,
 * proposes deploying the release on the SaaS cluster as a pull request against main that
 * changes the chart version in deploy/flux/ (deploy.ts; deploy/flux/README.md).
 *
 *   bun scripts/release/deploy-pr.ts --plan <plan.json> --published <published.json>
 *
 * Environment: GITHUB_TOKEN (contents: write for the branch, pull-requests: write for the PR,
 * actions: write to start ci.yml on the branch), GITHUB_API_URL and GITHUB_SERVER_URL.
 * The checkout must be the released commit: the shape values are copied from it.
 *
 * When the repository does not let Actions open PRs, the job still succeeds: the release is
 * published and verified, the branch is pushed, and the job summary carries the compare link,
 * title and body for a person to open the PR, with a warning annotation on the run. Any other
 * failure fails the job (and the release's commit status).
 */

import { appendFileSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { describeOutcome, githubApi, proposeDeploy } from "./deploy.ts";
import { log, parseOptions, requireCheckout, runStage, setOutput, warn } from "./exec.ts";
import { CHART_SHAPE_VALUES } from "./flux.ts";
import { ReleaseError, parsePlan, parsePublished, requirePublishedAsPlanned } from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");

await runStage("deploy-pr", async () => {
  const options = parseOptions(Bun.argv.slice(2), { plan: {}, published: {} });
  const plan = parsePlan(readFileSync(options.plan ?? "", "utf8"));
  const published = parsePublished(readFileSync(options.published ?? "", "utf8"));
  requirePublishedAsPlanned(plan, published);
  await requireCheckout(plan.commit);
  const token = process.env.GITHUB_TOKEN;
  if (!token) {
    throw new ReleaseError("GITHUB_TOKEN must be set to propose the deploy PR");
  }
  const outcome = await proposeDeploy(githubApi(token, process.env.GITHUB_API_URL || "https://api.github.com"), {
    plan,
    published,
    shapeValues: readFileSync(join(ROOT, CHART_SHAPE_VALUES), "utf8"),
    serverUrl: process.env.GITHUB_SERVER_URL || "https://github.com",
  });
  if (outcome.kind === "proposed") {
    setOutput("pull-request", outcome.pullRequest);
  }
  const { summary, warning } = describeOutcome(outcome, plan.version);
  if (warning === undefined) {
    log("deploy-pr", summary);
  } else {
    warn("deploy-pr", warning);
  }
  const file = process.env.GITHUB_STEP_SUMMARY;
  if (file) {
    appendFileSync(file, `### Deploy\n\n${summary}\n`);
  }
});
