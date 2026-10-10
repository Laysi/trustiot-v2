// Service-to-service authorization e2e test (Laysi/trustiot-v2#63): installs the chart into an
// existing namespace of any cluster with Istio ambient and checks decision 27's two layers on
// real traffic. Calls are made from inside the services' own pods, so ztunnel sees each
// service's ServiceAccount, plus two test pods: one in the mesh with a ServiceAccount that is
// not a service's, and one outside the mesh.
//
//   1. both layers: the listed call and the ingress gateway's calls pass, every other call fails,
//      and every pod's health checks pass under default deny;
//   2. NetworkPolicies removed: the AuthorizationPolicies alone still refuse every unlisted call;
//   3. both removed: the same calls now pass, which proves the calls themselves work;
//   4. NetworkPolicies back, AuthorizationPolicies still removed: the NetworkPolicies alone refuse
//      every unlisted call;
//   5. both back: the installed state again.
//
// README.md in this directory lists the prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-63 E2E_IDENTITY_IMAGE=<image> bun test ./deploy/chart/tests/e2e/mesh.e2e.ts
//
// Not part of `bun test deploy/chart` (the file name has no .test. suffix): it needs a cluster.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Subprocess } from "bun";
import {
  GATEWAY,
  IDENTITY_IMAGE,
  KEEP,
  NAMESPACE,
  OBJECT_PREFIX,
  PULL_POLICY,
  RELEASE,
  REPORT_DIR,
  freePort,
  helm,
  imageValues,
  installRelease,
  kubectl,
  kubectlArgs,
  run,
  uninstallRelease,
  waitFor,
} from "./cluster";

// How long a call may take before it counts as refused. A NetworkPolicy drops packets, so a
// refused call ends in a timeout; ztunnel resets refused connections right away.
const CALL_TIMEOUT_MS = Number(process.env.E2E_CALL_TIMEOUT_MS ?? "5000");
// How long a policy change may take to reach ztunnel and the NetworkPolicy controller.
const SETTLE_MS = Number(process.env.E2E_SETTLE_MS ?? "90000");
const HOSTS = { app: "app.saas.trustiot.test", gateway: "gw.saas.trustiot.test" };
// Services of tests/e2e/values-mesh-e2e.yaml; all run the identity double's image.
const SERVICES = ["identity", "assets", "certs", "mqtt-bridge", "alerts"] as const;
type Service = (typeof SERVICES)[number];
const SUFFIX = crypto.randomUUID().slice(0, 8);
const TEST_POD_LABEL = "trustiot-mesh-e2e";

// ---------------------------------------------------------------------------------------------
// Calls from inside a pod: `kubectl exec` runs Bun in the caller's container, so the connection
// leaves from the caller's network namespace with the caller's mesh identity.

type CallResult = { outcome: "response" | "error" | "timeout"; status?: number; code?: string; message?: string; ms: number };
type Caller = { name: string; target: string[] };

const serviceCaller = (service: Service): Caller => ({
  name: service,
  target: [`deployment/${OBJECT_PREFIX}-${service}`, "--container", service],
});
const testPodCaller = (name: string, pod: string): Caller => ({ name, target: [`pod/${pod}`, "--container", "client"] });
const OUTSIDER_POD = `mesh-e2e-outsider-${SUFFIX}`;
const STRANGER_POD = `mesh-e2e-stranger-${SUFFIX}`;
const outsider = testPodCaller("outsider (not in the mesh)", OUTSIDER_POD);
const stranger = testPodCaller("stranger (in the mesh, ServiceAccount default)", STRANGER_POD);

function callScript(url: string, host?: string): string {
  return `const started = performance.now();
const ms = () => Math.round(performance.now() - started);
try {
  const response = await fetch(${JSON.stringify(url)}, {
    headers: ${JSON.stringify(host ? { host } : {})},
    signal: AbortSignal.timeout(${CALL_TIMEOUT_MS}),
  });
  await response.text();
  console.log(JSON.stringify({ outcome: "response", status: response.status, ms: ms() }));
} catch (error) {
  const timeout = error?.name === "TimeoutError" || error?.name === "AbortError";
  console.log(JSON.stringify({ outcome: timeout ? "timeout" : "error", code: error?.code, message: String(error?.message ?? error), ms: ms() }));
}`;
}

function call(caller: Caller, url: string, host?: string): CallResult {
  const output = run(kubectlArgs("exec", ...caller.target, "--", "bun", "-e", callScript(url, host)));
  const line = output
    .trim()
    .split("\n")
    .reverse()
    .find((l) => l.startsWith("{") && l.includes('"outcome"'));
  if (!line) throw new Error(`no result from ${caller.name} calling ${url}:\n${output}`);
  return JSON.parse(line) as CallResult;
}

const passed = (result: CallResult) => result.outcome === "response" && result.status === 200;

const serviceUrl = (service: Service) => `http://${OBJECT_PREFIX}-${service}:8080/healthz`;
function podIp(selector: string): string {
  const ip = kubectl("get", "pods", "-l", selector, "-o", "jsonpath={.items[0].status.podIP}").trim();
  if (!ip) throw new Error(`no pod IP for ${selector}`);
  return ip;
}
const servicePodUrl = (service: Service) =>
  `http://${podIp(`app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=${service}`)}:8080/healthz`;

type Case = { name: string; caller: Caller; url: () => string; host?: string };
// The one listed call (mesh.calls in values-mesh-e2e.yaml).
const LISTED: Case = { name: "assets -> certs (listed)", caller: serviceCaller("assets"), url: () => serviceUrl("certs") };
// Calls nobody listed. Each fails while either layer is in place.
const UNLISTED: Case[] = [
  { name: "certs -> assets (the listed call reversed)", caller: serviceCaller("certs"), url: () => serviceUrl("assets") },
  { name: "assets -> alerts (alerts has no caller)", caller: serviceCaller("assets"), url: () => serviceUrl("alerts") },
  { name: "mqtt-bridge -> identity (only the gateway may call it)", caller: serviceCaller("mqtt-bridge"), url: () => serviceUrl("identity") },
  { name: "stranger -> certs (mesh identity that is not a service)", caller: stranger, url: () => serviceUrl("certs") },
  { name: "outsider -> certs Service (no mesh identity)", caller: outsider, url: () => serviceUrl("certs") },
  { name: "outsider -> certs pod IP, application port", caller: outsider, url: () => servicePodUrl("certs") },
];
// The port the gateway pod listens on for the Service's port 80 (the probe below bypasses the
// Service, so it needs the targetPort).
function gatewayListenerPort(): number {
  const targetPort = kubectl("get", "service", `${GATEWAY}-istio`, "-o", "jsonpath={.spec.ports[?(@.port==80)].targetPort}").trim();
  const port = Number(targetPort);
  if (!Number.isInteger(port) || port <= 0) throw new Error(`gateway Service port 80 has no numeric targetPort: ${JSON.stringify(targetPort)}`);
  return port;
}
// Only the ingress NetworkPolicy keeps an in-cluster pod away from the gateway (the gateway is
// not a mesh workload), so this call shows whether the NetworkPolicies are in force.
const NETWORK_POLICY_PROBE: Case = {
  name: "outsider -> ingress gateway pod (only the host may connect)",
  caller: outsider,
  url: () => `http://${podIp(`gateway.networking.k8s.io/gateway-name=${GATEWAY}`)}:${gatewayListenerPort()}/healthz`,
  host: HOSTS.app,
};

const results: Record<string, unknown>[] = [];
function check(phase: string, testCase: Case): CallResult {
  const url = testCase.url();
  const result = call(testCase.caller, url, testCase.host);
  results.push({ phase, call: testCase.name, url, ...result });
  return result;
}
// Polls until the call has the expected result, for policy changes that are still propagating.
async function settle(phase: string, testCase: Case, pass: boolean): Promise<void> {
  await waitFor(`${testCase.name} to ${pass ? "pass" : "fail"} (${phase})`, () => passed(check(`${phase} (settling)`, testCase)) === pass, SETTLE_MS);
}
function expectUnlisted(phase: string, pass: boolean) {
  for (const testCase of UNLISTED) {
    const result = check(phase, testCase);
    expect({ call: testCase.name, passed: passed(result) }).toEqual({ call: testCase.name, passed: pass });
  }
}

// ---------------------------------------------------------------------------------------------
// The ingress gateway's own calls, through kubectl port-forward into the gateway pod: public
// paths only, so the test needs no ext_authz provider.

let tunnel: Subprocess | undefined;
let gatewayPort = 0;
async function openTunnel(): Promise<void> {
  tunnel?.kill();
  gatewayPort = await freePort();
  tunnel = Bun.spawn(kubectlArgs("port-forward", `service/${GATEWAY}-istio`, `${gatewayPort}:80`), {
    stdout: "ignore",
    stderr: "inherit",
  });
  await waitFor("the tunnel to the ingress gateway", async () => {
    try {
      const socket = await Bun.connect({ hostname: "127.0.0.1", port: gatewayPort, socket: { data() {} } });
      socket.end();
      return true;
    } catch {
      return false;
    }
  });
}
const GATEWAY_CALLS: [string, string, string][] = [
  ["gateway -> identity (public /healthz)", HOSTS.app, "/healthz"],
  ["gateway -> certs (public /certs/renew)", HOSTS.gateway, "/certs/renew"],
  ["gateway -> mqtt-bridge (public /mqtt)", HOSTS.gateway, "/mqtt"],
];
async function expectGatewayReachesBackends(phase: string) {
  for (const [name, host, path] of GATEWAY_CALLS) {
    // kubectl port-forward exits when its connection to the API server drops; reopen it.
    if (!tunnel || tunnel.exitCode !== null || tunnel.signalCode !== null) await openTunnel();
    const url = `http://${host}${path}`;
    let status = 0;
    try {
      const response = await fetch(`http://127.0.0.1:${gatewayPort}${path}`, { headers: { host }, signal: AbortSignal.timeout(15_000) });
      status = response.status;
      await response.text();
      results.push({ phase, call: name, url, outcome: "response", status });
    } catch (error) {
      results.push({ phase, call: name, url, outcome: "error", message: String(error) });
    }
    expect({ call: name, status }).toEqual({ call: name, status: 200 });
  }
}

// ---------------------------------------------------------------------------------------------
// Pods and health checks.

type PodStatus = { name: string; ready: boolean; restarts: number };
function podStatuses(selector: string): PodStatus[] {
  const list = JSON.parse(kubectl("get", "pods", "-l", selector, "-o", "json")) as {
    items: { metadata: { name: string }; status: { conditions?: { type: string; status: string }[]; containerStatuses?: { restartCount: number }[] } }[];
  };
  return list.items.map((pod) => ({
    name: pod.metadata.name,
    ready: pod.status.conditions?.some((c) => c.type === "Ready" && c.status === "True") ?? false,
    restarts: (pod.status.containerStatuses ?? []).reduce((sum, c) => sum + c.restartCount, 0),
  }));
}
function expectHealthyPods(phase: string) {
  const pods = [
    ...podStatuses(`app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component in (${SERVICES.join(",")})`),
    ...podStatuses(`gateway.networking.k8s.io/gateway-name=${GATEWAY}`),
  ];
  results.push({ phase, pods });
  // One pod per service plus the gateway's two replicas.
  expect(pods.length).toBe(SERVICES.length + 2);
  for (const pod of pods) expect(pod).toEqual({ name: pod.name, ready: true, restarts: 0 });
}

// ---------------------------------------------------------------------------------------------
// Removing and restoring the release's policies. The objects come back exactly as the chart
// rendered them (helm get manifest), with Helm's ownership annotations.

type Manifest = { apiVersion: string; kind: string; metadata: { name: string; labels?: Record<string, string>; annotations?: Record<string, string> } };
let manifests: Manifest[] = [];
const networkPolicies = () => manifests.filter((m) => m.kind === "NetworkPolicy");
// The service policies only; the ingress gateway's cookie check and /internal denial stay.
const servicePolicies = () =>
  manifests.filter((m) => m.kind === "AuthorizationPolicy" && !m.metadata.name.startsWith(`${OBJECT_PREFIX}-ingress-`));

const RESOURCES: Record<string, string> = {
  NetworkPolicy: "networkpolicies.networking.k8s.io",
  AuthorizationPolicy: "authorizationpolicies.security.istio.io",
};
function remove(objects: Manifest[]) {
  for (const object of objects) kubectl("delete", RESOURCES[object.kind] ?? object.kind, object.metadata.name, "--wait=true");
}
function restore(objects: Manifest[]) {
  const items = objects.map((object) => ({
    ...object,
    metadata: {
      ...object.metadata,
      annotations: { ...object.metadata.annotations, "meta.helm.sh/release-name": RELEASE, "meta.helm.sh/release-namespace": NAMESPACE },
    },
  }));
  run(kubectlArgs("apply", "-f", "-"), false, JSON.stringify({ apiVersion: "v1", kind: "List", items }));
}

// ---------------------------------------------------------------------------------------------
// Install and tear down.

let releaseCreated = false;
const testPods: string[] = [];

function testPod(name: string, dataplaneMode: "ambient" | "none") {
  testPods.push(name);
  const pod = {
    apiVersion: "v1",
    kind: "Pod",
    metadata: { name, labels: { "app.kubernetes.io/part-of": TEST_POD_LABEL, "istio.io/dataplane-mode": dataplaneMode } },
    spec: {
      automountServiceAccountToken: false,
      enableServiceLinks: false,
      terminationGracePeriodSeconds: 1,
      securityContext: { runAsNonRoot: true, runAsUser: 65532, runAsGroup: 65532, seccompProfile: { type: "RuntimeDefault" } },
      containers: [
        {
          name: "client",
          image: IDENTITY_IMAGE,
          imagePullPolicy: PULL_POLICY,
          securityContext: { allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: ["ALL"] } },
          resources: { requests: { cpu: "10m", memory: "32Mi" }, limits: { memory: "128Mi" } },
        },
      ],
    },
  };
  run(kubectlArgs("create", "-f", "-"), false, JSON.stringify(pod));
}

beforeAll(async () => {
  installRelease(
    ["values-saas.yaml", "tests/values/saas-instance.yaml", "tests/e2e/values-e2e.yaml", "tests/e2e/values-mesh-e2e.yaml"],
    SERVICES.flatMap((service) => imageValues(service, IDENTITY_IMAGE)),
  );
  releaseCreated = true;
  const parsed: unknown = Bun.YAML.parse(helm("get", "manifest", RELEASE));
  manifests = (Array.isArray(parsed) ? parsed : [parsed]).filter(
    (m): m is Manifest => typeof m === "object" && m !== null && "kind" in m,
  );
  expect(networkPolicies().length).toBe(SERVICES.length + 2);
  expect(servicePolicies().length).toBe(SERVICES.length);
  kubectl("wait", `gateway/${GATEWAY}`, "--for=condition=Programmed", "--timeout=180s");
  kubectl("rollout", "status", `deployment/${GATEWAY}-istio`, "--timeout=180s");
  testPod(OUTSIDER_POD, "none");
  testPod(STRANGER_POD, "ambient");
  kubectl("wait", `pod/${OUTSIDER_POD}`, `pod/${STRANGER_POD}`, "--for=condition=Ready", "--timeout=180s");
  await openTunnel();
}, 600_000);

afterAll(() => {
  tunnel?.kill();
  const errors: string[] = [];
  // A failed report write must not skip the cleanup below.
  if (REPORT_DIR) {
    try {
      mkdirSync(REPORT_DIR, { recursive: true });
      writeFileSync(join(REPORT_DIR, "mesh-e2e.json"), `${JSON.stringify(results, null, 2)}\n`);
    } catch (error) {
      errors.push(`report: ${String(error)}`);
    }
  }
  if (KEEP) {
    if (errors.length) throw new Error(errors.join("\n"));
    return;
  }
  for (const pod of testPods) {
    try {
      kubectl("delete", "pod", pod, "--ignore-not-found", "--wait=false");
    } catch (error) {
      errors.push(String(error));
    }
  }
  if (releaseCreated) {
    try {
      uninstallRelease();
    } catch (error) {
      errors.push(String(error));
    }
  }
  if (errors.length) throw new Error(`cleanup failed:\n${errors.join("\n")}`);
}, 300_000);

// ---------------------------------------------------------------------------------------------
// The phases run in order; each one starts from the state the previous one left.

describe("service-to-service authorization (decision 27)", () => {
  test("1. both layers: the listed call and the gateway pass, unlisted calls fail, health checks pass under default deny", async () => {
    const phase = "both layers";
    expectHealthyPods(phase);
    await settle(phase, LISTED, true);
    expect(passed(check(phase, NETWORK_POLICY_PROBE))).toBe(false);
    await expectGatewayReachesBackends(phase);
    expectUnlisted(phase, false);
  }, 600_000);

  test("2. NetworkPolicies removed: the AuthorizationPolicies alone refuse every unlisted call", async () => {
    const phase = "AuthorizationPolicy only";
    remove(networkPolicies());
    await settle(phase, NETWORK_POLICY_PROBE, true);
    expect(passed(check(phase, LISTED))).toBe(true);
    await expectGatewayReachesBackends(phase);
    expectUnlisted(phase, false);
  }, 600_000);

  test("3. both removed: the unlisted calls pass, so the layers are what refused them", async () => {
    const phase = "neither layer";
    remove(servicePolicies());
    for (const testCase of UNLISTED) await settle(phase, testCase, true);
    expectUnlisted(phase, true);
    expect(passed(check(phase, LISTED))).toBe(true);
  }, 600_000);

  test("4. NetworkPolicies back, AuthorizationPolicies still removed: the NetworkPolicies alone refuse every unlisted call", async () => {
    const phase = "NetworkPolicy only";
    restore(networkPolicies());
    await settle(phase, NETWORK_POLICY_PROBE, false);
    for (const testCase of UNLISTED) await settle(phase, testCase, false);
    expectUnlisted(phase, false);
    expect(passed(check(phase, LISTED))).toBe(true);
    await expectGatewayReachesBackends(phase);
  }, 600_000);

  test("5. both layers back: the installed state, and every pod is still healthy", async () => {
    const phase = "both layers again";
    restore(servicePolicies());
    // Nothing observable changes while the NetworkPolicies already refuse the unlisted calls, so
    // give ztunnel the settle time it needs before the final checks.
    await Bun.sleep(5_000);
    expectUnlisted(phase, false);
    expect(passed(check(phase, LISTED))).toBe(true);
    await expectGatewayReachesBackends(phase);
    expectHealthyPods(phase);
  }, 600_000);
});
