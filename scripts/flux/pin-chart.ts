/**
 * Writes one cluster's chart source and shape values for a release, exactly as release.yml's
 * deploy PR writes them (scripts/release/deploy.ts, deploy/flux/README.md):
 *
 *   bun scripts/flux/pin-chart.ts --cluster demo --version 0.0.0-rc.8 --digest sha256:<64 hex>
 *
 * For what the deploy PR does not do: the first version of a new cluster, or moving a single
 * cluster back to an earlier release while the others stay. The digest is the chart digest on
 * the release's GitHub Release page (the table "發佈內容", checked by the verify job); Flux
 * still installs the chart only with release.yml's keyless signature at that tag, so a wrong
 * digest makes the OCIRepository fail, never installs another chart. The shape values come from
 * the release tag in this clone (`git fetch origin --tags` first).
 */

import { writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { parseOptions, run, runStage } from "../release/exec.ts";
import { type ClusterName, chartSourceFile, renderChartSource, renderShapeValues, shapeValuesFile } from "../release/flux.ts";
import { CHART_REGISTRY, ReleaseError, isDigest, parseReleaseTag } from "../release/lib.ts";
import { parseCluster } from "./lib.ts";

const ROOT = resolve(import.meta.dir, "..", "..");
const REPOSITORY = "Laysi/trustiot-v2";

/** The two files of one cluster for a release, keyed by their repository path. */
export function pinnedFiles(cluster: ClusterName, version: string, digest: string, shapeValues: string): Map<string, string> {
  parseReleaseTag(`v${version}`);
  if (!isDigest(digest)) {
    throw new ReleaseError("--digest must be a sha256 digest (sha256:<64 hex digits>)");
  }
  return new Map([
    [chartSourceFile(cluster), renderChartSource({ version, digest, repository: REPOSITORY, chartRepository: `${CHART_REGISTRY}/trustiot` })],
    [shapeValuesFile(cluster), renderShapeValues(version, shapeValues)],
  ]);
}

if (import.meta.main) {
  await runStage("pin-chart", async () => {
    const options = parseOptions(Bun.argv.slice(2), { cluster: {}, version: {}, digest: {} });
    const cluster = parseCluster(options.cluster);
    const version = options.version ?? "";
    const tag = parseReleaseTag(`v${version}`).tag;
    const shape = await run(["git", "show", `${tag}:deploy/chart/values-saas.yaml`], { cwd: ROOT });
    for (const [path, content] of pinnedFiles(cluster, version, options.digest ?? "", shape.stdout)) {
      writeFileSync(join(ROOT, path), content);
      console.log(`wrote ${path}`);
    }
  });
}
