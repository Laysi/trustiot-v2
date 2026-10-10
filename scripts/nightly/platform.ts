// The platform components the chart expects to find in a cluster (deploy/istio/README.md): the
// Gateway API CRDs and Istio ambient with the ext_authz provider the ingress gateway refers to.
// Installed once on the nightly cluster, the way the SaaS host installs them before the chart.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { retry, run, RunLog } from "./exec.ts";

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

/** Builds the e2e test's identity double and loads it into the cluster nodes. */
async function loadIdentityDouble(platform: Platform): Promise<void> {
  const context = join(platform.repoRoot, "deploy", "chart", "tests", "e2e", "identity-double");
  await retry(platform.log, 2, 10_000, () =>
    run(platform.log, ["docker", "build", "--tag", IDENTITY_DOUBLE_IMAGE, "--file", join(context, "Containerfile"), context], { timeoutMs: 600_000 }),
  );
  await run(platform.log, [platform.k3d, "image", "import", IDENTITY_DOUBLE_IMAGE, "--cluster", platform.cluster], { timeoutMs: 300_000 });
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
