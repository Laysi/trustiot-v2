// Service-to-service authorization e2e test (Laysi/trustiot-v2#63): installs the chart into an
// existing namespace of any cluster with Istio ambient and checks decision 27's two layers on
// real traffic. Calls are made from inside the services' own pods, so ztunnel sees each
// service's ServiceAccount, plus three test pods: one in the mesh with a ServiceAccount that is
// not a service's (stranger), one outside the mesh (outsider), and one that carries the assets
// service's labels but not its ServiceAccount (imposter), which only the AuthorizationPolicy
// can tell apart.
//
//   1. both layers: the listed calls and the ingress gateway's calls pass, every other call fails,
//      and every pod's health checks pass under default deny;
//   2. AuthorizationPolicies removed: the NetworkPolicies alone refuse every unlisted call (the
//      imposter now gets through, which shows the AuthorizationPolicies are gone);
//   3. both removed: the unlisted calls pass, which proves the calls themselves work;
//   4. AuthorizationPolicies back, NetworkPolicies still removed: the AuthorizationPolicies alone
//      refuse every unlisted call;
//   5. both back: the installed state again.
//
// Calls on behalf of users (Laysi/trustiot-v2#106): every call from a pod carries a user's
// internal identity token, signed with the release's own key and checked against the published
// keys every service reads, so the refusals above hold even for a caller that holds a genuine
// user's token. A token addressed to the callee goes with a direct call; the caller's own token
// goes to the identity service's token exchange, on the exchange's own port (token-exchange) and
// Service (<release>-identity-exchange), which the chart lets every service that calls another one
// on behalf of users reach (assets here), and no other service (mqtt-bridge, certs). Such a
// service reaches the exchange's port only, never the identity service's API port with its
// ext_authz check: the AuthorizationPolicies split the ports, so that call fails while they are
// in place and passes without them (the NetworkPolicies admit assets's pods to the identity
// pod's HBONE port for the exchange). The services run the identity double, which
// verifies no token, so every refusal here is the mesh's (L4): the services' and the token
// exchange's own checks (the call list, one hop only) are tested in Rust
// (services/identity/tests/ext_authz.rs and services/identity/src/exchange/tests.rs); this test
// shows what the mesh lets through, that the token arrives with the listed call, and that the
// identity service got the call list.
//
// The AI gateway's admin port (Laysi/trustiot-v2#607) is split the same way: its own Service and
// policies (<release>-ai-gateway-admin) admit the identity service alone, on the admin port only,
// and the gateway's own policies admit its listed caller (the assistant) on the API port only. So
// the identity service reaches the admin API and not the API port, the assistant the API port
// and not the admin API (by the admin Service or the pod's address), and every other service and
// test pod neither. The NetworkPolicies admit the identity service's and the assistant's pods to
// the gateway pod's HBONE port, so the port split is the AuthorizationPolicies' alone, like the
// identity service's API port above. The gateway runs the identity double too, which serves the
// echo on 8080 and 8082 (the chart's admin port).
//
// The NetworkPolicy-only phase comes before any unlisted call has ever succeeded: a
// NetworkPolicy does not cut connections that are already open, and ztunnel keeps HBONE
// connections open for reuse, so restoring NetworkPolicies after phase 3 would not refuse calls
// over those connections. ztunnel checks the AuthorizationPolicy on every proxied connection, so
// phase 4 does not depend on the order.
//
// README.md in this directory lists the prerequisites; run from the repository root:
//
//   E2E_NAMESPACE=e2e-63 E2E_IDENTITY_IMAGE=<image> bun test ./deploy/chart/tests/e2e/mesh.e2e.ts
//
// Not part of `bun test deploy/chart` (the file name has no .test. suffix): it needs a cluster.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Subprocess } from "bun";
import {
  GATEWAY,
  IDENTITY_IMAGE,
  KEEP,
  MIGRATE_IMAGE,
  NAMESPACE,
  OBJECT_PREFIX,
  PULL_POLICY,
  RELEASE,
  REPORT_DIR,
  freePort,
  helm,
  imageValues,
  installRelease,
  migrateImageValues,
  kubectl,
  kubectlArgs,
  run,
  uninstallRelease,
  waitFor,
} from "./cluster";
import { type PublishedKeySet, type SigningKeysDocument, TokenSigner, type UserClaims, verifyToken } from "./internal-token.ts";
import { type CallResult, callScript, parseCallOutput } from "./mesh-call.ts";
import { sendRaw } from "./raw-client.ts";
import { withTunnelRecovery } from "./tunnel-recovery.ts";

// How long a call may take before it counts as refused. Depending on the NetworkPolicy
// controller, a refused connection is rejected right away or dropped (a timeout); ztunnel resets
// refused connections right away.
const CALL_TIMEOUT_MS = Number(process.env.E2E_CALL_TIMEOUT_MS ?? "5000");
// How long a policy change may take to reach ztunnel and the NetworkPolicy controller.
const SETTLE_MS = Number(process.env.E2E_SETTLE_MS ?? "90000");
const HOSTS = { app: "app.saas.trustiot.test", gateway: "gw.saas.trustiot.test" };
// Services of tests/e2e/values-mesh-e2e.yaml; all run the identity double's image.
const SERVICES = ["identity", "assets", "certs", "mqtt-bridge", "alerts", "assistant", "ai-gateway"] as const;
type Service = (typeof SERVICES)[number];
const SUFFIX = crypto.randomUUID().slice(0, 8);
const TEST_POD_LABEL = "trustiot-mesh-e2e";
// The identity double echoes every other path with the request's headers (identity-double/server.ts).
const ECHO_PATH = "/v1/mesh-e2e";
// The identity service's token exchange (contract::api::identity::ExchangeToken), served on the
// identity service's port token-exchange (values.yaml), apart from its API port, through a
// Service of its own (templates/token-exchange.yaml).
const EXCHANGE_PATH = "/internal/token-exchange";
const EXCHANGE_SERVICE = "identity-exchange";
const API_PORT = 8080;
const EXCHANGE_PORT = 8082;
// The call list the chart hands the identity service for values-mesh-e2e.yaml: the assistant's
// call to the AI gateway is not on it (decision 28), nor the identity service's to the admin API.
const ON_BEHALF_CALLS = { assets: ["certs"] };
// The AI gateway's admin API (Laysi/trustiot-v2#607): a Service of its own on a fixed port
// (templates/ai-gateway-admin.yaml), forwarding to the gateway's container port admin, 8082 by
// default and the port the double serves its echo on besides 8080.
const ADMIN_SERVICE = "ai-gateway-admin";
const ADMIN_PORT = 8082;
// The address the chart gives the identity service (TRUSTIOT_AI_GATEWAY_ADMIN_URL).
const ADMIN_URL = `http://${OBJECT_PREFIX}-${ADMIN_SERVICE}.${NAMESPACE}.svc:${ADMIN_PORT}`;

// ---------------------------------------------------------------------------------------------
// Secrets the AI gateway's settings name (values-mesh-e2e.yaml): the provider keys and the
// Postgres CA, which the test creates before the install, and the gateway's database password,
// which the chart declares empty for the migration Job to fill (off here) and the test fills
// instead. The double reads none of them; their values exist only for this run, in files of a
// private directory, so they never appear in a command line.
const AI_KEY_SECRET = "trustiot-e2e-ai";
const POSTGRES_CA_SECRET = "trustiot-e2e-postgres-ca";
const PASSWORD_SECRET = `${OBJECT_PREFIX}-ai-gateway-password`;
const PASSWORD_KEY = "ai_gateway_app";
// How long the filler waits for Helm to declare the password Secret: within the install's own
// time limit (install.ts, WAIT_ARGS).
const FILL_TIMEOUT_MS = 300_000;
const work = mkdtempSync(join(tmpdir(), "trustiot-mesh-e2e-"));
const createdSecrets: string[] = [];

function secretFile(name: string, value: string): string {
  const path = join(work, name);
  writeFileSync(path, value, { mode: 0o600 });
  return path;
}
const randomHex = (bytes: number) => Array.from(crypto.getRandomValues(new Uint8Array(bytes)), (b) => b.toString(16).padStart(2, "0")).join("");

// The Secrets are this run's own: one left by an earlier run (the password Secret is kept on
// uninstall, helm.sh/resource-policy keep) would be adopted, so refuse it.
function refuseLeftoverSecrets() {
  for (const name of [AI_KEY_SECRET, POSTGRES_CA_SECRET, PASSWORD_SECRET]) {
    if (run(kubectlArgs("get", "secret", name, "--ignore-not-found", "--output", "name"), false).trim() !== "") {
      throw new Error(`${NAMESPACE} already has Secret ${name} from an earlier run; delete it first`);
    }
  }
}

function createInputSecrets() {
  createdSecrets.push(AI_KEY_SECRET);
  kubectl("create", "secret", "generic", AI_KEY_SECRET, `--from-file=api-key=${secretFile("ai-key", `mesh-e2e-${randomHex(24)}`)}`);
  createdSecrets.push(POSTGRES_CA_SECRET);
  // Not a certificate: the double connects to no database. The gateway's pod only mounts it.
  kubectl("create", "secret", "generic", POSTGRES_CA_SECRET, `--from-file=ca.crt=${secretFile("ca.crt", "mesh.e2e.ts: no database\n")}`);
}

// installRelease blocks until the pods are ready, and the gateway's pod cannot start before its
// password Secret has the key, so a separate process fills it once Helm has declared it (the
// migration Job's part in a real install) and restarts the gateway's pod instead of waiting for
// the kubelet's next retry.
function startPasswordFiller(): Subprocess {
  createdSecrets.push(PASSWORD_SECRET);
  const patch = secretFile("password-patch.json", JSON.stringify({ data: { [PASSWORD_KEY]: Buffer.from(randomHex(32)).toString("base64") } }));
  const commands = {
    get: kubectlArgs("get", "secret", PASSWORD_SECRET),
    patch: kubectlArgs("patch", "secret", PASSWORD_SECRET, "--type", "merge", "--patch-file", patch),
    restart: kubectlArgs("delete", "pod", "--selector", `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/component=ai-gateway`, "--wait=false"),
  };
  const script = `const commands = JSON.parse(process.env.MESH_E2E_FILL ?? "{}");
const deadline = Date.now() + ${FILL_TIMEOUT_MS};
while (Date.now() < deadline) {
  if (Bun.spawnSync({ cmd: commands.get, stdout: "ignore", stderr: "ignore" }).exitCode === 0) {
    const patched = Bun.spawnSync({ cmd: commands.patch, stdout: "ignore", stderr: "pipe" });
    if (patched.exitCode !== 0) {
      console.error("mesh.e2e.ts: patching the AI gateway's password Secret failed: " + patched.stderr.toString());
      process.exit(1);
    }
    const restarted = Bun.spawnSync({ cmd: commands.restart, stdout: "ignore", stderr: "pipe" });
    if (restarted.exitCode !== 0) console.error("mesh.e2e.ts: restarting the AI gateway's pod failed: " + restarted.stderr.toString());
    process.exit(0);
  }
  await Bun.sleep(1000);
}
console.error("mesh.e2e.ts: timed out waiting for Helm to declare the AI gateway's password Secret");
process.exit(1);`;
  return Bun.spawn([process.execPath, "-e", script], {
    env: { ...process.env, MESH_E2E_FILL: JSON.stringify(commands) },
    stdout: "ignore",
    stderr: "inherit",
  });
}

// ---------------------------------------------------------------------------------------------
// Users' tokens (internal-token.ts). The signing key stays in this process; tokens reach the
// calling pod on stdin and are never printed or recorded.

const USER: UserClaims = { sub: crypto.randomUUID(), tenant: { id: crypto.randomUUID(), role: "viewer" }, platform_admin: false };
let signer: TokenSigner | undefined;
let publishedKeys: PublishedKeySet | undefined;

function loadSigner() {
  const secret = JSON.parse(kubectl("get", "secret", `${OBJECT_PREFIX}-identity-signing-keys`, "--output", "json")) as { data?: Record<string, string> };
  const document = JSON.parse(Buffer.from(secret.data?.["signing-keys.json"] ?? "", "base64").toString() || "{}") as SigningKeysDocument;
  const configMap = JSON.parse(kubectl("get", "configmap", `${OBJECT_PREFIX}-internal-identity-keys`, "--output", "json")) as { data?: Record<string, string> };
  publishedKeys = JSON.parse(configMap.data?.["jwks.json"] ?? "{}") as PublishedKeySet;
  signer = TokenSigner.from({ keys: document.keys ?? [] }, { keys: publishedKeys.keys ?? [] });
}

function userToken(audience: string): string {
  if (!signer || !publishedKeys) throw new Error("no signing key loaded");
  const token = signer.token(USER, audience);
  // The services accept it: it verifies against the key set they mount.
  verifyToken(token, publishedKeys);
  return token;
}

// ---------------------------------------------------------------------------------------------
// Calls from inside a pod: `kubectl exec` runs Bun in the caller's container (mesh-call.ts), so
// the connection leaves from the caller's network namespace with the caller's mesh identity.

type Caller = { name: string; target: string[] };

const serviceCaller = (service: Service): Caller => ({
  name: service,
  target: [`deployment/${OBJECT_PREFIX}-${service}`, "--container", service],
});
const testPodCaller = (name: string, pod: string): Caller => ({ name, target: [`pod/${pod}`, "--container", "client"] });
const OUTSIDER_POD = `mesh-e2e-outsider-${SUFFIX}`;
const STRANGER_POD = `mesh-e2e-stranger-${SUFFIX}`;
const IMPOSTER_POD = `mesh-e2e-imposter-${SUFFIX}`;
const outsider = testPodCaller("outsider (not in the mesh)", OUTSIDER_POD);
const stranger = testPodCaller("stranger (in the mesh, ServiceAccount default)", STRANGER_POD);
const imposter = testPodCaller("imposter (assets labels, ServiceAccount default)", IMPOSTER_POD);

type CallOptions = { host?: string; token?: string; body?: unknown };

function call(caller: Caller, url: string, options: CallOptions = {}): CallResult {
  const withToken = options.token !== undefined;
  const script = callScript(url, { host: options.host, withToken, body: options.body, timeoutMs: CALL_TIMEOUT_MS });
  // The token goes on stdin (`-i`), so it is in no command line, process listing or error
  // message; the script prints only whether the callee received it.
  const output = run(kubectlArgs("exec", ...(withToken ? ["-i"] : []), ...caller.target, "--", "bun", "-e", script), false, options.token);
  const result = parseCallOutput(output);
  if (!result) throw new Error(`no result from ${caller.name} calling ${url}:\n${output}`);
  return result;
}

const passed = (result: CallResult) => result.outcome === "response" && result.status === 200;

// The chart's own pods; the imposter test pod shares their selector labels but not this one.
const CHART_PODS = `app.kubernetes.io/instance=${RELEASE},app.kubernetes.io/part-of=trustiot`;
const serviceUrl = (service: Service, path = ECHO_PATH, port = API_PORT) => `http://${OBJECT_PREFIX}-${service}:${port}${path}`;
function podIp(selector: string): string {
  const ip = kubectl("get", "pods", "-l", selector, "-o", "jsonpath={.items[0].status.podIP}").trim();
  if (!ip) throw new Error(`no pod IP for ${selector}`);
  return ip;
}
const servicePodUrl = (service: Service, port = API_PORT) =>
  `http://${podIp(`${CHART_PODS},app.kubernetes.io/component=${service}`)}:${port}${ECHO_PATH}`;
// The AI gateway's admin API through its own Service, at the address the identity service gets.
const adminUrl = () => `${ADMIN_URL}${ECHO_PATH}`;

// `token`: the audience of the user's token the call carries: the callee for a direct call, the
// caller itself for the token exchange. `body`: a JSON body (POST).
type Case = { name: string; caller: Caller; url: () => string; host?: string; token?: Service; body?: unknown };
// A direct call with a user's token addressed to the callee.
const direct = (name: string, caller: Caller, callee: Service, url = () => serviceUrl(callee)): Case => ({ name, caller, url, token: callee });
// A service asks the identity service for a token addressed to `audience`, presenting its own.
const exchange = (name: string, caller: Service, audience: Service): Case => ({
  name,
  caller: serviceCaller(caller),
  url: () => `http://${OBJECT_PREFIX}-${EXCHANGE_SERVICE}:${EXCHANGE_PORT}${EXCHANGE_PATH}`,
  token: caller,
  body: { audience },
});
// A call without a user's token: the AI gateway takes none on either port (decision 28).
const plain = (name: string, caller: Caller, url: () => string): Case => ({ name, caller, url });
// The listed calls: mesh.calls in values-mesh-e2e.yaml, the token exchange the chart adds for
// the calls on behalf of users among them, and the identity service's call to the AI gateway's
// admin API, which the chart adds (Laysi/trustiot-v2#607).
const LISTED: Case[] = [
  direct("assets -> certs with a user's token for certs (listed)", serviceCaller("assets"), "certs"),
  exchange("assets -> identity token exchange for certs (assets calls certs on behalf of users)", "assets", "certs"),
  plain("assistant -> ai-gateway API port (listed: the assistant forwards model calls)", serviceCaller("assistant"), () => serviceUrl("ai-gateway")),
  plain("identity -> ai-gateway admin Service (the identity service alone reaches the admin API)", serviceCaller("identity"), adminUrl),
];
// Calls nobody listed, each with a user's token the callee would accept. Each fails while either
// layer is in place.
const UNLISTED: Case[] = [
  direct("certs -> assets with a user's token for assets (the listed call reversed)", serviceCaller("certs"), "assets"),
  direct("assets -> alerts with a user's token for alerts (alerts has no caller)", serviceCaller("assets"), "alerts"),
  exchange("mqtt-bridge -> identity token exchange (mqtt-bridge calls no service on behalf of users)", "mqtt-bridge", "assets"),
  exchange("certs -> identity token exchange (certs is only called on behalf of users)", "certs", "assets"),
  direct("stranger -> certs with a user's token for certs (mesh identity that is not a service)", stranger, "certs"),
  direct("outsider -> certs Service with a user's token for certs (no mesh identity)", outsider, "certs"),
  direct("outsider -> certs pod IP, application port, with a user's token for certs", outsider, "certs", () => servicePodUrl("certs")),
  // The AI gateway: alerts may call it (decision 28) but does not list it here; assets, a
  // service that may never call it, and the test pods reach neither port.
  plain("alerts -> ai-gateway API port (alerts does not list ai-gateway)", serviceCaller("alerts"), () => serviceUrl("ai-gateway")),
  plain("alerts -> ai-gateway admin Service (only the identity service reaches the admin API)", serviceCaller("alerts"), adminUrl),
  plain("assets -> ai-gateway admin Service (only the identity service reaches the admin API)", serviceCaller("assets"), adminUrl),
  plain("stranger -> ai-gateway admin Service (mesh identity that is not a service)", stranger, adminUrl),
  plain("outsider -> ai-gateway pod IP, admin port (no mesh identity)", outsider, () => servicePodUrl("ai-gateway", ADMIN_PORT)),
];
// Calls a NetworkPolicy admits because the caller's pods may reach the callee's pods on another
// port, which only the AuthorizationPolicies refuse: the NetworkPolicies cannot see ports inside
// HBONE. Each passes while the AuthorizationPolicies are removed (phases 2 and 3).
// - A caller on behalf of users at the identity service's API port, where the ext_authz check is:
//   the NetworkPolicies admit assets's pods for the token exchange (Laysi/trustiot-v2#106, M2 of
//   the security review).
// - The AI gateway's admin API (Laysi/trustiot-v2#607): the assistant, which reaches the API port,
//   through the admin Service and the pod's address; and the identity service, which reaches the
//   admin port, at the API port.
const PORT_SPLIT: Case[] = [
  direct(
    "assets -> identity API port with a user's token for identity (assets may reach the token exchange only)",
    serviceCaller("assets"),
    "identity",
  ),
  plain("assistant -> ai-gateway admin Service (the assistant may reach the API port only)", serviceCaller("assistant"), adminUrl),
  plain("assistant -> ai-gateway pod IP, admin port (the assistant may reach the API port only)", serviceCaller("assistant"), () =>
    servicePodUrl("ai-gateway", ADMIN_PORT),
  ),
  plain("identity -> ai-gateway API port (the identity service may reach the admin port only)", serviceCaller("identity"), () => serviceUrl("ai-gateway")),
];
function expectPortSplit(phase: string, pass: boolean) {
  for (const testCase of PORT_SPLIT) {
    const result = check(phase, testCase);
    expect({ call: testCase.name, passed: passed(result) }).toEqual({ call: testCase.name, passed: pass });
  }
}
// A pod with the listed caller's labels but another ServiceAccount: the NetworkPolicy admits it
// (it selects pods by label), the AuthorizationPolicy does not (it checks the mTLS identity).
const IMPOSTER: Case = direct("imposter -> certs with a user's token for certs (assets labels, not its ServiceAccount)", imposter, "certs");
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
  // A fresh token per call: the identity service's lifetime is 60 seconds.
  const token = testCase.token === undefined ? undefined : userToken(testCase.token);
  const result = call(testCase.caller, url, { host: testCase.host, token, body: testCase.body });
  results.push({ phase, call: testCase.name, url, ...(testCase.token ? { tokenFor: testCase.token } : {}), ...result });
  return result;
}
// Polls until the call has the expected result, for policy changes that are still propagating.
async function settle(phase: string, testCase: Case, pass: boolean): Promise<void> {
  await waitFor(`${testCase.name} to ${pass ? "pass" : "fail"} (${phase})`, () => passed(check(`${phase} (settling)`, testCase)) === pass, SETTLE_MS);
}
// The listed calls pass, and the callee received the user's token when the call carried one.
function expectListed(phase: string) {
  for (const testCase of LISTED) {
    const result = check(phase, testCase);
    const withToken = testCase.token !== undefined;
    expect({ call: testCase.name, passed: passed(result), tokenEchoed: result.tokenEchoed }).toEqual({
      call: testCase.name,
      passed: true,
      tokenEchoed: withToken ? true : undefined,
    });
  }
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
      // Same recovery as ingress.e2e.ts: a tunnel that dies while the request is in flight is
      // reopened and the request is sent once more (Laysi/trustiot-v2#261).
      const response = await withTunnelRecovery(
        () => sendRaw({ host: "127.0.0.1", port: gatewayPort, tls: false }, { target: path, hostHeader: host }),
        { tunnel: () => tunnel, reopen: openTunnel, log: (message) => console.error(message) },
      );
      status = response.status;
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
    ...podStatuses(`${CHART_PODS},app.kubernetes.io/component in (${SERVICES.join(",")})`),
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
// The CronJob that rotates the internal identity signing key by age (Laysi/trustiot-v2#429)
// renders with the identity service and has a NetworkPolicy without ingress and an
// AuthorizationPolicy without rules of its own. Nothing calls its pod, so the phases leave both
// in place and they count toward neither layer below; beforeAll checks that they are there.
const ROTATION_COMPONENT = "identity-key-rotation";
const isRotation = (m: Manifest) => m.metadata.labels?.["app.kubernetes.io/component"] === ROTATION_COMPONENT;
const rotationPolicies = () => manifests.filter((m) => isRotation(m) && (m.kind === "NetworkPolicy" || m.kind === "AuthorizationPolicy"));
const networkPolicies = () => manifests.filter((m) => m.kind === "NetworkPolicy" && !isRotation(m));
// The service policies only; the ingress gateway's cookie check and /internal denial stay.
const servicePolicies = () =>
  manifests.filter((m) => m.kind === "AuthorizationPolicy" && !m.metadata.name.startsWith(`${OBJECT_PREFIX}-ingress-`) && !isRotation(m));

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

// A client pod running the identity double's image as a plain server. The imposter also carries
// the assets service's selector labels, and a readiness probe that never passes, so it never
// becomes an endpoint of the assets Service (the ReplicaSet does not adopt it either: its
// selector also needs pod-template-hash).
function testPod(name: string, dataplaneMode: "ambient" | "none", impersonate?: Service) {
  testPods.push(name);
  const impersonated = impersonate
    ? { "app.kubernetes.io/name": "trustiot", "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/component": impersonate }
    : {};
  const pod = {
    apiVersion: "v1",
    kind: "Pod",
    metadata: {
      name,
      labels: { ...impersonated, "app.kubernetes.io/part-of": TEST_POD_LABEL, "istio.io/dataplane-mode": dataplaneMode },
    },
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
          ...(impersonate ? { readinessProbe: { httpGet: { path: "/never-ready", port: 1 }, periodSeconds: 30 } } : {}),
        },
      ],
    },
  };
  run(kubectlArgs("create", "-f", "-"), false, JSON.stringify(pod));
}

beforeAll(async () => {
  refuseLeftoverSecrets();
  createInputSecrets();
  const filler = startPasswordFiller();
  try {
    installRelease(
      ["values-saas.yaml", "tests/values/saas-instance.yaml", "tests/e2e/values-e2e.yaml", "tests/e2e/values-mesh-e2e.yaml"],
      [...SERVICES.flatMap((service) => imageValues(service, IDENTITY_IMAGE)), ...migrateImageValues(MIGRATE_IMAGE)],
    );
  } finally {
    // A failed install has already removed itself; the filler must not outlive it.
    if (filler.exitCode === null) filler.kill();
  }
  releaseCreated = true;
  expect(await filler.exited, "the AI gateway's password Secret filler").toBe(0);
  const parsed: unknown = Bun.YAML.parse(helm("get", "manifest", RELEASE));
  manifests = (Array.isArray(parsed) ? parsed : [parsed]).filter(
    (m): m is Manifest => typeof m === "object" && m !== null && "kind" in m,
  );
  // One per service, the default deny, the ingress gateway's, the token exchange's, the certs
  // service's broker-certs port's (templates/certs-broker.yaml), which admits nothing here because
  // no MQTT broker runs, and the AI gateway's admin port's (templates/ai-gateway-admin.yaml).
  expect(networkPolicies().length).toBe(SERVICES.length + 5);
  // One per service, the token exchange's, the broker-certs port's and the admin port's.
  expect(servicePolicies().length).toBe(SERVICES.length + 3);
  for (const name of [`${OBJECT_PREFIX}-${EXCHANGE_SERVICE}`, `${OBJECT_PREFIX}-certs-broker`, `${OBJECT_PREFIX}-${ADMIN_SERVICE}`]) {
    expect(manifests.filter((m) => m.metadata.name === name).map((m) => m.kind).sort()).toEqual([
      "AuthorizationPolicy",
      "NetworkPolicy",
      "Service",
    ]);
  }
  // The identity service is enabled and internalIdentity.maxKeyAgeSeconds keeps its default, so
  // the rotation CronJob and its two policies render, all under the CronJob's name.
  const rotation = manifests.filter((m) => m.kind === "CronJob" && isRotation(m)).map((m) => m.metadata.name);
  expect(rotation).toHaveLength(1);
  expect(rotationPolicies().map((m) => `${m.kind}/${m.metadata.name}`).sort()).toEqual(
    [`AuthorizationPolicy/${rotation[0]}`, `NetworkPolicy/${rotation[0]}`],
  );
  // The identity service exchanges tokens along the calls on behalf of users that mesh.calls
  // lists (Laysi/trustiot-v2#106).
  const identity = manifests.find((m) => m.kind === "Deployment" && m.metadata.name === `${OBJECT_PREFIX}-identity`) as
    | (Manifest & { spec: { template: { spec: { containers: { env?: { name: string; value?: string }[] }[] } } } })
    | undefined;
  const onBehalf = identity?.spec.template.spec.containers[0]?.env?.find((e) => e.name === "TRUSTIOT_ON_BEHALF_CALLS")?.value;
  expect(JSON.parse(onBehalf ?? "null")).toEqual(ON_BEHALF_CALLS);
  // The identity service gets the admin API's address, which the identity -> admin call uses, and
  // the gateway listens on the pod's address for it (Laysi/trustiot-v2#607).
  expect(identity?.spec.template.spec.containers[0]?.env?.find((e) => e.name === "TRUSTIOT_AI_GATEWAY_ADMIN_URL")?.value).toBe(ADMIN_URL);
  const aiGateway = manifests.find((m) => m.kind === "Deployment" && m.metadata.name === `${OBJECT_PREFIX}-ai-gateway`) as
    | (Manifest & { spec: { template: { spec: { containers: { env?: { name: string; value?: string }[] }[] } } } })
    | undefined;
  expect(aiGateway?.spec.template.spec.containers[0]?.env?.find((e) => e.name === "AI_GATEWAY__ADMIN__LISTEN_ADDR")?.value).toBe(`0.0.0.0:${ADMIN_PORT}`);
  // The init container filled the signing keys before the identity pod started.
  loadSigner();
  results.push({ phase: "setup", signingKey: signer?.kid, onBehalfCalls: ON_BEHALF_CALLS });
  kubectl("wait", `gateway/${GATEWAY}`, "--for=condition=Programmed", "--timeout=180s");
  kubectl("rollout", "status", `deployment/${GATEWAY}-istio`, "--timeout=180s");
  testPod(OUTSIDER_POD, "none");
  testPod(STRANGER_POD, "ambient");
  testPod(IMPOSTER_POD, "ambient", "assets");
  kubectl("wait", `pod/${OUTSIDER_POD}`, `pod/${STRANGER_POD}`, "--for=condition=Ready", "--timeout=180s");
  kubectl("wait", `pod/${IMPOSTER_POD}`, "--for=condition=ContainersReady=false", "--timeout=180s");
  kubectl("wait", `pod/${IMPOSTER_POD}`, "--for=jsonpath={.status.containerStatuses[0].started}=true", "--timeout=180s");
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
  // The Secrets' values exist only in this run's files; nothing needs them after the install.
  rmSync(work, { recursive: true, force: true });
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
  // After the release: the Secrets this run created, and the password Secret, which Helm keeps
  // on uninstall (helm.sh/resource-policy keep). refuseLeftoverSecrets made sure each is this run's.
  for (const name of createdSecrets) {
    try {
      kubectl("delete", "secret", name, "--ignore-not-found");
    } catch (error) {
      errors.push(String(error));
    }
  }
  if (errors.length) throw new Error(`cleanup failed:\n${errors.join("\n")}`);
}, 300_000);

// ---------------------------------------------------------------------------------------------
// The phases run in order; each one starts from the state the previous one left.

describe("service-to-service authorization (decision 27)", () => {
  test("1. both layers: the listed calls and the gateway pass, unlisted calls fail, health checks pass under default deny", async () => {
    const phase = "both layers";
    expectHealthyPods(phase);
    for (const testCase of LISTED) await settle(phase, testCase, true);
    expectListed(phase);
    expect(passed(check(phase, NETWORK_POLICY_PROBE))).toBe(false);
    await expectGatewayReachesBackends(phase);
    expectUnlisted(phase, false);
    expect(passed(check(phase, IMPOSTER))).toBe(false);
    expectPortSplit(phase, false);
  }, 600_000);

  test("2. AuthorizationPolicies removed: the NetworkPolicies alone refuse every unlisted call", async () => {
    const phase = "NetworkPolicy only";
    remove(servicePolicies());
    // The NetworkPolicy admits the imposter by its labels, so once it gets through, the
    // AuthorizationPolicies are no longer enforced.
    await settle(phase, IMPOSTER, true);
    // Only the AuthorizationPolicies kept assets off the identity service's API port, the
    // assistant off the AI gateway's admin port and the identity service off its API port.
    for (const testCase of PORT_SPLIT) await settle(phase, testCase, true);
    expectPortSplit(phase, true);
    expectUnlisted(phase, false);
    expect(passed(check(phase, NETWORK_POLICY_PROBE))).toBe(false);
    expectListed(phase);
    await expectGatewayReachesBackends(phase);
  }, 600_000);

  test("3. both removed: the unlisted calls pass, so the layers are what refused them", async () => {
    const phase = "neither layer";
    remove(networkPolicies());
    await settle(phase, NETWORK_POLICY_PROBE, true);
    for (const testCase of UNLISTED) await settle(phase, testCase, true);
    expectUnlisted(phase, true);
    expectPortSplit(phase, true);
    expectListed(phase);
  }, 600_000);

  test("4. AuthorizationPolicies back, NetworkPolicies still removed: the AuthorizationPolicies alone refuse every unlisted call", async () => {
    const phase = "AuthorizationPolicy only";
    restore(servicePolicies());
    for (const testCase of [...UNLISTED, IMPOSTER, ...PORT_SPLIT]) await settle(phase, testCase, false);
    expectUnlisted(phase, false);
    expect(passed(check(phase, IMPOSTER))).toBe(false);
    expectPortSplit(phase, false);
    expect(passed(check(phase, NETWORK_POLICY_PROBE))).toBe(true);
    expectListed(phase);
    await expectGatewayReachesBackends(phase);
  }, 600_000);

  test("5. both layers back: the installed state, and every pod is still healthy", async () => {
    const phase = "both layers again";
    restore(networkPolicies());
    await settle(phase, NETWORK_POLICY_PROBE, false);
    expectUnlisted(phase, false);
    expect(passed(check(phase, IMPOSTER))).toBe(false);
    expectPortSplit(phase, false);
    expectListed(phase);
    await expectGatewayReachesBackends(phase);
    expectHealthyPods(phase);
  }, 600_000);
});
