// Nightly smoke test (docs/architecture/ci-release.md, nightly.yml): creates a k3d cluster,
// deploys the Helm chart once per values file (SaaS, then on-prem), checks every deployment
// and writes report.json, report.md and run.log to the output directory.
//
//   bun scripts/nightly/run.ts
//
// Environment:
//   K3S_IMAGE                    required; the k3s image of the cluster, pinned by digest
//   GATEWAY_API_VERSION          required; Gateway API release whose standard-install.yaml is applied
//   GATEWAY_CRDS_SHA256           required; SHA-256 of that file
//   ISTIO_VERSION                required; Istio release of the four Helm charts (deploy/istio/README.md)
//   CNPG_VERSION                 required; CloudNativePG release whose manifest is applied (deploy/cloudnative-pg/README.md)
//   CNPG_MANIFEST_SHA256         required; SHA-256 of that manifest
//   NIGHTLY_OUT                  output directory (default: ./nightly-out)
//   NIGHTLY_SIMULATE_FAILURE     `true` fails every shape on purpose after its install
//   NIGHTLY_IMAGE_CACHE_DIR      where nightly.yml restored the Rust image tarballs (image-cache.ts);
//                                unset: always build
//   NIGHTLY_IMAGE_CACHE_SAVE     `true` writes the built images there for nightly.yml to save
//   GITHUB_STEP_SUMMARY          when set, the Markdown report is appended to it
//
// Needs docker (for k3d and the images the tests build), openssl and the tools pinned in
// .prototools: bun, helm, k3d and kubectl. Building the Rust images from source (NIGHTLY_IMAGES:
// the migration Job's and the services', scripts/nightly/service-image.ts), which happens only
// when no cached image matches their inputs, also needs rustup and its cargo, zig, cargo-zigbuild
// and cargo-auditable.
// Exit status: 0 when every shape passed, 1 when a check failed, 2 on a usage error.
import { appendFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { exec, pinnedTool, retry, run, RunLog } from "./exec.ts";
import { isOk, renderMarkdown, type Report, type ShapeResult } from "./report.ts";
import { installPlatform } from "./platform.ts";
import { NIGHTLY_IMAGES } from "./image-cache.ts";
import { loadServiceImage } from "./service-image.ts";
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

export interface VersionPins {
  image: string;
  gatewayApiVersion: string;
  gatewayCrdsSha256: string;
  istioVersion: string;
  cnpgVersion: string;
  cnpgManifestSha256: string;
}

/** The versions the nightly installs, all pinned in nightly.yml; a missing or loose one is an error. */
export function readPins(env: Record<string, string | undefined>): VersionPins {
  const image = requireDigestPinned(env.K3S_IMAGE);
  const gatewayApiVersion = env.GATEWAY_API_VERSION ?? "";
  const gatewayCrdsSha256 = env.GATEWAY_CRDS_SHA256 ?? "";
  const istioVersion = env.ISTIO_VERSION ?? "";
  const cnpgVersion = env.CNPG_VERSION ?? "";
  const cnpgManifestSha256 = env.CNPG_MANIFEST_SHA256 ?? "";
  if (!/^v\d+\.\d+\.\d+$/.test(gatewayApiVersion)) throw new Error(`GATEWAY_API_VERSION must look like v1.6.1; got ${JSON.stringify(gatewayApiVersion)}`);
  if (!/^[0-9a-f]{64}$/.test(gatewayCrdsSha256)) throw new Error("GATEWAY_CRDS_SHA256 must be 64 lowercase hex digits");
  if (!/^\d+\.\d+\.\d+$/.test(istioVersion)) throw new Error(`ISTIO_VERSION must look like 1.30.5; got ${JSON.stringify(istioVersion)}`);
  if (!/^\d+\.\d+\.\d+$/.test(cnpgVersion)) throw new Error(`CNPG_VERSION must look like 1.30.1; got ${JSON.stringify(cnpgVersion)}`);
  if (!/^[0-9a-f]{64}$/.test(cnpgManifestSha256)) throw new Error("CNPG_MANIFEST_SHA256 must be 64 lowercase hex digits");
  return { image, gatewayApiVersion, gatewayCrdsSha256, istioVersion, cnpgVersion, cnpgManifestSha256 };
}

async function main(): Promise<number> {
  let pins: VersionPins;
  try {
    pins = readPins(process.env);
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    return 2;
  }
  const image = pins.image;
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
          // The k3s of a production install (SaaS and on-prem), whose only change is that Traefik is
          // off (decision 27): ServiceLB stays, because the on-prem shape exposes the ingress as a
          // LoadBalancer Service whose address k3s's ServiceLB assigns. No k3d load balancer node:
          // that is k3d's own way of publishing ports, which the tests do not use.
          "--no-lb",
          "--k3s-arg", "--disable=traefik@server:0",
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
    // k3s's addon controller creates CoreDNS a few seconds after the node is Ready, and rollout
    // status fails on an object that does not exist yet: wait for it to exist first.
    await run(log, [kubectl, "--namespace", "kube-system", "wait", "--for=create", "deployment/coredns", "--timeout=120s"], { ...kube, timeoutMs: 150_000 });
    await run(log, [kubectl, "--namespace", "kube-system", "rollout", "status", "deployment/coredns", "--timeout=180s"], { ...kube, timeoutMs: 200_000 });
    await run(log, [kubectl, "version"], { ...kube, timeoutMs: 60_000 });

    await installPlatform(
      {
        repoRoot: REPO_ROOT, outDir, kubeconfig, helm, kubectl, k3d, cluster: CLUSTER, log,
        gatewayApiVersion: pins.gatewayApiVersion, gatewayCrdsSha256: pins.gatewayCrdsSha256, istioVersion: pins.istioVersion,
        cnpgVersion: pins.cnpgVersion, cnpgManifestSha256: pins.cnpgManifestSha256,
      },
      shapes.map((shape) => shape.name),
    );
    // The Rust images deploy/chart/tests/values/smoke.yaml names: the migration Job's
    // (tools/migrate) and the services the smoke calls through the ingress (identity, assets).
    const cacheDir = process.env.NIGHTLY_IMAGE_CACHE_DIR;
    const cache = cacheDir ? { cache: { dir: cacheDir, save: process.env.NIGHTLY_IMAGE_CACHE_SAVE === "true" } } : {};
    for (const name of NIGHTLY_IMAGES) {
      log.line(`== platform: ${name} image`);
      await loadServiceImage({ repoRoot: REPO_ROOT, log, k3d, cluster: CLUSTER }, name, cache);
    }

    for (const shape of shapes) {
      shapeResults.push(await smokeShape({ repoRoot: REPO_ROOT, chartDir: CHART_DIR, outDir, kubeconfig, helm, kubectl, log, simulateFailure }, shape));
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
