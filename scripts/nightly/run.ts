// Nightly smoke test (docs/architecture/ci-release.md, nightly.yml): creates a k3d cluster,
// deploys the Helm chart once per values file (SaaS, then on-prem), checks every deployment
// and writes report.json, report.md and run.log to the output directory.
//
//   bun scripts/nightly/run.ts
//
// Environment:
//   K3S_IMAGE                    required; the k3s image of the cluster, pinned by digest
//   NIGHTLY_OUT                  output directory (default: ./nightly-out)
//   NIGHTLY_SIMULATE_FAILURE     `true` fails every shape on purpose after its install
//   GITHUB_STEP_SUMMARY          when set, the Markdown report is appended to it
//
// Needs docker (for k3d) and the tools pinned in .prototools: helm, k3d and kubectl.
// Exit status: 0 when every shape passed, 1 when a check failed, 2 on a usage error.
import { appendFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { exec, pinnedTool, retry, run, RunLog } from "./exec.ts";
import { isOk, renderMarkdown, type Report, type ShapeResult } from "./report.ts";
import { discoverShapes, smokeShape } from "./smoke.ts";

const REPO_ROOT = resolve(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");
const CLUSTER = "nightly";

/** The pinned image reference must carry a digest: a moving tag would make a night unrepeatable. */
export function requireDigestPinned(image: string | undefined): string {
  if (image === undefined || !/^[^\s@]+@sha256:[0-9a-f]{64}$/.test(image)) {
    throw new Error(
      `K3S_IMAGE must be an image reference pinned by digest, such as rancher/k3s:v1.36.5-k3s1@sha256:<64 hex digits>; got ${JSON.stringify(image)}`,
    );
  }
  return image;
}

async function main(): Promise<number> {
  let image: string;
  try {
    image = requireDigestPinned(process.env.K3S_IMAGE);
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    return 2;
  }
  const outDir = resolve(process.env.NIGHTLY_OUT ?? "nightly-out");
  mkdirSync(outDir, { recursive: true });
  const log = new RunLog(join(outDir, "run.log"));
  const simulateFailure = process.env.NIGHTLY_SIMULATE_FAILURE === "true";
  // The kubeconfig holds the throwaway cluster's credentials: a 0700 directory outside the
  // output directory, which is uploaded as an artifact.
  const secretDir = mkdtempSync(join(tmpdir(), "nightly-kube-"));
  const kubeconfig = join(secretDir, "kubeconfig");

  const startedAt = new Date().toISOString();
  const shapeResults: ShapeResult[] = [];
  let infraError: string | undefined;
  let expectedShapes = 0;
  let k3d = "k3d";

  try {
    const [helm, kubectl] = await Promise.all([pinnedTool("helm", REPO_ROOT), pinnedTool("kubectl", REPO_ROOT)]);
    k3d = await pinnedTool("k3d", REPO_ROOT);
    const shapes = discoverShapes(CHART_DIR);
    expectedShapes = shapes.length;
    log.line(`shapes: ${shapes.map((s) => s.name).join(", ")}; k3s image ${image}`);

    await retry(log, 3, 15_000, async () => {
      await exec([k3d, "cluster", "delete", CLUSTER], { timeoutMs: 120_000 });
      await run(
        log,
        [
          k3d, "cluster", "create", CLUSTER,
          "--image", image,
          "--servers", "1", "--agents", "0",
          // The chart's Services are ClusterIP and the smoke test reaches them through the API
          // server, so no load balancer node, Traefik, ServiceLB or metrics-server is needed.
          "--no-lb",
          "--k3s-arg", "--disable=traefik@server:0",
          "--k3s-arg", "--disable=servicelb@server:0",
          "--k3s-arg", "--disable=metrics-server@server:0",
          "--kubeconfig-update-default=false",
          "--kubeconfig-switch-context=false",
          "--wait", "--timeout", "240s",
        ],
        { timeoutMs: 300_000 },
      );
    });
    const config = await run(log, [k3d, "kubeconfig", "get", CLUSTER], { timeoutMs: 60_000, quiet: true });
    writeFileSync(kubeconfig, config.stdout, { mode: 0o600 });
    const kube = { env: { KUBECONFIG: kubeconfig } };
    await run(log, [kubectl, "wait", "--for=condition=Ready", "node", "--all", "--timeout=180s"], { ...kube, timeoutMs: 200_000 });
    await run(log, [kubectl, "--namespace", "kube-system", "rollout", "status", "deployment/coredns", "--timeout=180s"], { ...kube, timeoutMs: 200_000 });
    await run(log, [kubectl, "version"], { ...kube, timeoutMs: 60_000 });

    for (const shape of shapes) {
      shapeResults.push(await smokeShape({ chartDir: CHART_DIR, outDir, kubeconfig, helm, kubectl, log, simulateFailure }, shape));
    }
  } catch (error) {
    infraError = error instanceof Error ? error.message : String(error);
    log.line(`INFRASTRUCTURE FAILURE: ${infraError}`);
  } finally {
    const deleted = await exec([k3d, "cluster", "delete", CLUSTER], { timeoutMs: 180_000 });
    log.line(`k3d cluster delete exited with ${deleted.code}`);
    if (deleted.code !== 0) {
      // A cluster that is left behind is a failed run, even when every check passed.
      const reason = `k3d cluster delete ${CLUSTER} ${deleted.timedOut ? "timed out" : `exited with ${deleted.code}`}: ${deleted.stderr.trim()}`;
      infraError = infraError === undefined ? reason : `${infraError}\n${reason}`;
    }
    rmSync(secretDir, { recursive: true, force: true });
  }

  const partial = { infraError, shapes: shapeResults };
  const report: Report = {
    startedAt,
    finishedAt: new Date().toISOString(),
    k3sImage: image,
    ...(infraError === undefined ? {} : { infraError }),
    shapes: shapeResults,
    ok: isOk(partial, expectedShapes),
  };
  const markdown = renderMarkdown(report);
  writeFileSync(join(outDir, "report.json"), `${JSON.stringify(report, null, 2)}\n`);
  writeFileSync(join(outDir, "report.md"), markdown);
  const summary = process.env.GITHUB_STEP_SUMMARY;
  if (summary !== undefined && summary !== "") appendFileSync(summary, markdown);
  log.line(report.ok ? "nightly smoke test passed" : "nightly smoke test FAILED");
  return report.ok ? 0 : 1;
}

if (import.meta.main) {
  process.exit(await main());
}
