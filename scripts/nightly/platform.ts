// The platform components the chart expects to find in a cluster (deploy/istio/README.md): the
// Gateway API CRDs and Istio ambient with the ext_authz provider the ingress gateway refers to.
// Installed once on the nightly cluster, the way the SaaS host installs them before the chart.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { containerBuildArgs } from "../lib/build-context.ts";

import { exec, retry, run, RunLog } from "./exec.ts";
import type { ExecOptions, ExecResult } from "./exec.ts";

export const ISTIO_CHART_REPOSITORY = "https://istio-release.storage.googleapis.com/charts";
/** The mesh extension provider that the chart's AuthorizationPolicy names (ingress.authzProvider). */
export const BASE_PROVIDER = "trustiot-identity";
export const IDENTITY_DOUBLE_IMAGE = "localhost/trustiot-e2e/identity-double:nightly";

export interface Platform {
  repoRoot: string;
  outDir: string;
  kubeconfig: string;
  helm: string;
  kubectl: string;
  k3d: string;
  cluster: string;
  log: RunLog;
  /** Gateway API release, for example `v1.6.1`, and the SHA-256 of its standard-install.yaml. */
  gatewayApiVersion: string;
  gatewayCrdsSha256: string;
  /** Istio release, for example `1.30.5`, used for all four charts. */
  istioVersion: string;
}

/** The ingress e2e test installs the chart in its own namespace, with its own identity service. */
export function e2eNamespaceOf(shape: string): string {
  return `nightly-e2e-${shape}`;
}

/** Each e2e namespace gets its own provider, because a provider names one identity service. */
export function providerNameOf(shape: string): string {
  return `trustiot-e2e-${shape}`;
}

interface Provider {
  name: string;
  envoyExtAuthzHttp: { service: string; [key: string]: unknown };
  [key: string]: unknown;
}

/**
 * istiod values for the nightly: deploy/istio/istiod-values.yaml with its one provider repeated
 * for the identity service of each e2e namespace, so the settings (fail closed, 0.5 s timeout,
 * 503 on error, headers) stay the repository's own and cannot drift from what the test checks.
 */
export function istiodOverlay(base: string, shapes: string[]): string {
  const values = Bun.YAML.parse(base) as { meshConfig?: { extensionProviders?: Provider[] }; [key: string]: unknown };
  const provider = values.meshConfig?.extensionProviders?.find((candidate) => candidate.name === BASE_PROVIDER);
  if (provider === undefined) {
    throw new Error(`deploy/istio/istiod-values.yaml has no extension provider named ${BASE_PROVIDER}`);
  }
  const providers = shapes.map((shape) => ({
    ...structuredClone(provider),
    name: providerNameOf(shape),
    envoyExtAuthzHttp: {
      ...structuredClone(provider.envoyExtAuthzHttp),
      service: `${BASE_PROVIDER}.${e2eNamespaceOf(shape)}.svc.cluster.local`,
    },
  }));
  return Bun.YAML.stringify({ ...values, meshConfig: { ...values.meshConfig, extensionProviders: providers } }, null, 2);
}

/** Checks a downloaded file against its pinned SHA-256; the CRDs are cluster-wide and run as admin. */
export function verifySha256(data: Uint8Array, expected: string, what: string): void {
  const actual = new Bun.CryptoHasher("sha256").update(data).digest("hex");
  if (actual !== expected.toLowerCase()) {
    throw new Error(`${what} has SHA-256 ${actual}, expected ${expected}`);
  }
}

async function installGatewayApi(platform: Platform): Promise<void> {
  const url = `https://github.com/kubernetes-sigs/gateway-api/releases/download/${platform.gatewayApiVersion}/standard-install.yaml`;
  const body = await retry(platform.log, 3, 5_000, async () => {
    platform.log.line(`GET ${url}`);
    const response = await fetch(url);
    if (!response.ok) throw new Error(`GET ${url} returned ${response.status}`);
    return new Uint8Array(await response.arrayBuffer());
  });
  verifySha256(body, platform.gatewayCrdsSha256, `Gateway API ${platform.gatewayApiVersion} standard-install.yaml`);
  const file = join(platform.outDir, "gateway-api-standard-install.yaml");
  writeFileSync(file, body);
  const kube = { env: { KUBECONFIG: platform.kubeconfig } };
  // Server-side: the CRDs are too large for the annotation client-side apply keeps.
  await run(platform.log, [platform.kubectl, "apply", "--server-side", "--filename", file], { ...kube, timeoutMs: 180_000, quiet: true });
  await run(
    platform.log,
    [platform.kubectl, "wait", "--for=condition=Established", "crd/gateways.gateway.networking.k8s.io", "crd/httproutes.gateway.networking.k8s.io", "--timeout=120s"],
    { ...kube, timeoutMs: 150_000 },
  );
}

async function installIstio(platform: Platform, shapes: string[]): Promise<void> {
  const istioDir = join(platform.repoRoot, "deploy", "istio");
  const overlay = join(platform.outDir, "istiod-nightly.yaml");
  writeFileSync(overlay, istiodOverlay(readFileSync(join(istioDir, "istiod-values.yaml"), "utf8"), shapes));
  const platformValues = ["-f", join(istioDir, "values-k3s.yaml")];
  const kube = { env: { KUBECONFIG: platform.kubeconfig } };
  // The four charts of deploy/istio/README.md, in its order, from the official repository.
  const charts: { release: string; chart: string; extra: string[] }[] = [
    { release: "istio-base", chart: "base", extra: [] },
    { release: "istiod", chart: "istiod", extra: ["-f", overlay] },
    { release: "istio-cni", chart: "cni", extra: [] },
    { release: "ztunnel", chart: "ztunnel", extra: [] },
  ];
  for (const { release, chart, extra } of charts) {
    await retry(platform.log, 2, 10_000, () =>
      run(
        platform.log,
        [
          platform.helm, "upgrade", "--install", release, chart,
          "--repo", ISTIO_CHART_REPOSITORY, "--version", platform.istioVersion,
          "--namespace", "istio-system", "--create-namespace",
          ...platformValues, ...extra,
          "--wait", "--timeout", "8m",
        ],
        { ...kube, timeoutMs: 9 * 60_000 },
      ),
    );
  }
}

/**
 * istiod registers the `istio` GatewayClass after it has started, not as part of the helm release,
 * and `kubectl wait` on an object that does not exist yet fails at once. Wait for it to exist, and
 * for istiod to accept it, before any chart with a Gateway is installed.
 */
async function waitForGatewayClass(platform: Platform): Promise<void> {
  const kube = { env: { KUBECONFIG: platform.kubeconfig } };
  await run(platform.log, [platform.kubectl, "wait", "--for=create", "gatewayclass/istio", "--timeout=120s"], { ...kube, timeoutMs: 150_000 });
  await run(platform.log, [platform.kubectl, "wait", "--for=condition=Accepted", "gatewayclass/istio", "--timeout=120s"], { ...kube, timeoutMs: 150_000 });
}

/** The commands the image import runs; tests replace them, so no cluster or docker is needed. */
export interface ImportCommands {
  run: (log: RunLog, cmd: string[], options?: ExecOptions) => Promise<ExecResult>;
  exec: (cmd: string[], options?: ExecOptions) => Promise<ExecResult>;
}

export const IMPORT_ATTEMPTS = 3;
const IMPORT_PAUSE_MS = 5_000;

interface K3dNode {
  name?: string;
  role?: string;
  runtimeLabels?: Record<string, string>;
}

/** The server and agent nodes of a k3d cluster, from `k3d node list -o json`: the nodes with a containerd. */
export async function clusterNodeNames(
  platform: Pick<Platform, "k3d" | "cluster">,
  commands: Pick<ImportCommands, "exec"> = { exec },
): Promise<string[]> {
  const cmd = [platform.k3d, "node", "list", "--output", "json"];
  const result = await commands.exec(cmd, { timeoutMs: 60_000 });
  if (result.code !== 0) throw new Error(`\`${cmd.join(" ")}\` ${result.timedOut ? "timed out" : `exited with ${result.code}`}: ${result.stderr.trim()}`);
  let nodes: K3dNode[];
  try {
    nodes = JSON.parse(result.stdout) as K3dNode[];
  } catch {
    throw new Error(`\`${cmd.join(" ")}\` did not print JSON: ${result.stdout.slice(0, 200)}`);
  }
  const names = nodes
    .filter((node) => node.runtimeLabels?.["k3d.cluster"] === platform.cluster && (node.role === "server" || node.role === "agent"))
    .map((node) => node.name)
    .filter((name): name is string => name !== undefined && name !== "");
  if (names.length === 0) throw new Error(`k3d lists no server or agent node for cluster ${platform.cluster}`);
  return names;
}

/**
 * Imports a local docker image into every node of the k3d cluster, and proves it arrived.
 *
 * `k3d image import` is not proof: in its default tools-node mode the nodes import a tarball from
 * a shared volume, and k3d 5.9.0 only logs a node's failure (`ERRO failed to import images in node
 * ...: ctr: open /k3d/images/...tar: no such file or directory`), prints "Successfully imported" and
 * exits 0 (Laysi/trustiot-v2#335; seen in the PR smoke of 2026-10-11, where the image never reached
 * the node and the e2e's identity Deployment, pullPolicy IfNotPresent and a `localhost` registry
 * that cannot be pulled from, waited 5 minutes). So after each import every server and agent node
 * is asked for the image (`crictl inspecti`, which reads containerd's k8s.io namespace, the one
 * the kubelet uses), a node without it fails the attempt, and the last attempt's failure names the
 * nodes and the image. This check is the fix.
 *
 * Mitigation: `--mode direct` streams `docker save` into each node's `ctr image import`, with no
 * tools node and no shared volume (the tarball race above), and k3d returns a node's error instead
 * of logging it. It is a valid mode here because the docker daemon is local to the runner (what
 * `--mode auto` would pick too).
 */
export async function importImageIntoCluster(
  platform: Pick<Platform, "k3d" | "cluster" | "log">,
  image: string,
  commands: ImportCommands = { run, exec },
  { attempts = IMPORT_ATTEMPTS, pauseMs = IMPORT_PAUSE_MS }: { attempts?: number; pauseMs?: number } = {},
): Promise<void> {
  await retry(platform.log, attempts, pauseMs, async () => {
    await commands.run(platform.log, [platform.k3d, "image", "import", image, "--cluster", platform.cluster, "--mode", "direct"], { timeoutMs: 300_000 });
    const nodes = await clusterNodeNames(platform, commands);
    const missing: string[] = [];
    for (const node of nodes) {
      const result = await commands.exec(["docker", "exec", node, "crictl", "inspecti", image], { timeoutMs: 60_000 });
      if (result.code === 0) {
        platform.log.line(`image ${image} is present on node ${node}`);
      } else {
        const reason = result.timedOut ? "timed out" : (result.stderr.trim() || result.stdout.trim() || `exited with ${result.code}`).split("\n").at(-1);
        platform.log.line(`image ${image} is NOT present on node ${node}: ${reason}`);
        missing.push(node);
      }
    }
    if (missing.length > 0) {
      throw new Error(
        `k3d image import succeeded, but image ${image} is missing on node(s) ${missing.join(", ")} of cluster ${platform.cluster} (checked ${nodes.length} node(s): ${nodes.join(", ")})`,
      );
    }
    platform.log.line(`image ${image} confirmed on ${nodes.length} node(s): ${nodes.join(", ")}`);
  });
}

/** Builds the e2e test's identity double and loads it into the cluster nodes. */
async function loadIdentityDouble(platform: Platform): Promise<void> {
  const context = join(platform.repoRoot, "deploy", "chart", "tests", "e2e", "identity-double");
  await retry(platform.log, 2, 10_000, () =>
    // containerBuildArgs checks the context and the Containerfile first (docs/architecture/secrets.md).
    run(platform.log, ["docker", ...containerBuildArgs(context, ["--tag", IDENTITY_DOUBLE_IMAGE, "--file", join(context, "Containerfile")])], {
      timeoutMs: 600_000,
    }),
  );
  await importImageIntoCluster(platform, IDENTITY_DOUBLE_IMAGE);
}

/** Installs everything the chart and its e2e test need, in the order the chart needs it. */
export async function installPlatform(platform: Platform, shapes: string[]): Promise<void> {
  mkdirSync(platform.outDir, { recursive: true });
  platform.log.line("== platform: Gateway API CRDs");
  await installGatewayApi(platform);
  platform.log.line("== platform: Istio ambient");
  await installIstio(platform, shapes);
  await waitForGatewayClass(platform);
  platform.log.line("== platform: identity double image");
  await loadIdentityDouble(platform);
}
