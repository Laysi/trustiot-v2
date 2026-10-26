// The dashboard check's isolation in the nightly cluster (Laysi/trustiot-v2#150): the service runs
// pages a model wrote in a headless Chromium without Chromium's own sandbox, so the chart gives its
// pod no way out and lets only the AI assistant in (services/dashboard-check/README.md, 「威脅模型」;
// deploy/chart/README.md, 「儀表板檢查」). The chart tests check the rendered policies; this check
// proves them on a running cluster, with Istio ambient and the k3s NetworkPolicy controller:
//
// - from inside the dashboard check's pod nothing answers: a cluster DNS name does not resolve
//   (the NetworkPolicy refuses the cluster DNS, and ambient DNS capture is off for the pod), and
//   the Kubernetes API server (its Service and its endpoints), the OpenTelemetry collector and an
//   address outside the cluster send back no byte; its own health port on the pod's loopback does
//   answer, so the probe itself works there;
// - from the identity service's pod, which may open any connection, the same probes resolve and
//   answer (the controls that show each target is up and that the probe reaches it), while the
//   dashboard check's Service and its pod address send back nothing;
// - from the assistant's pod, the one caller, a check of the sandbox's `correct` fixture page
//   answers 200 with the verdict `passed`.
//
// Each probe runs in an ephemeral container (`kubectl debug`, restricted profile) of the pod it
// probes from, so its connections leave from that pod's network namespace with its mesh identity,
// as the service's own would. kubectl stays attached to the container and hands back what it
// printed; the container starts probing only once kubectl has attached and sent it a line, so no
// output is lost, and the run never reads a container's log. The container's image is the nightly's Postgres image, which is on
// the node already (platform.ts) and has bash with its /dev/tcp connections, getent, timeout, head
// and base64. A connection counts as answered when any byte comes back: with Istio ambient the
// pod's ztunnel accepts every connection first and closes it when it cannot reach the destination,
// so a successful connect proves nothing.
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { exec, run } from "./exec.ts";
import type { ExecOptions, ExecResult, RunLog } from "./exec.ts";
import { servicePortOf } from "./manifests.ts";
import type { K8sObject } from "./manifests.ts";
import { POSTGRES_IMAGE } from "./platform.ts";

/** The service's key under `services` and its pods' `app.kubernetes.io/component`. */
export const DASHBOARD_CHECK = "dashboard-check";
/** The check's route (crates/contract/src/api/dashboard/check.rs, CHECK_ROUTE). */
export const CHECK_ROUTE = "/internal/dashboard-check/v1/check";
/** The service's health port (a container port; the Service has none) and readiness path. */
export const HEALTH_PORT_NAME = "health";
export const READY_PATH = "/readyz";
/**
 * An address outside the cluster that answers plain HTTP: Cloudflare's public resolver on port 80.
 * The identity pod's control shows that the cluster reaches it, so silence from the dashboard
 * check's pod is its NetworkPolicy, not a runner without a way out.
 */
export const OUTSIDE = { host: "1.1.1.1", port: 80 } as const;
/** A name every pod with DNS resolves; with a final dot, so no search domain is tried first. */
export const CLUSTER_NAME = "kubernetes.default.svc.cluster.local.";
/** The image of the probe containers (see the top of this file). */
export const PROBE_IMAGE = POSTGRES_IMAGE;
/**
 * How long one probe may take, in seconds: a refused name lookup waits out the resolver's
 * attempts (glibc's default is two of 5 s), and a connection the policy drops ends when ztunnel
 * gives up or at this limit, with nothing back either way. The controls answer well within it.
 */
export const PROBE_LIMIT_S = 30;
/** How long the probe container may take to start, which kubectl waits for before it attaches. */
export const PROBE_START_LIMIT_S = 180;
/** How long `kubectl debug` may take from creating the container to its end (every probe runs at once). */
export const PROBE_CONTAINER_LIMIT_MS = (PROBE_START_LIMIT_S + 2 * PROBE_LIMIT_S + 60) * 1000;
/** How much of an answer a probe keeps. */
export const ANSWER_LIMIT_BYTES = 65_536;

/** One probe: a name lookup, or a connection that sends `send` and keeps what comes back. */
export type Probe =
  | { kind: "dns"; id: string; name: string }
  | { kind: "tcp"; id: string; host: string; port: number; send: string };

/** What a probe saw. `answered.bytes` is the answer as Latin-1 text, one character per byte. */
export type ProbeResult = { kind: "resolved"; address: string } | { kind: "unresolved" } | { kind: "answered"; bytes: string } | { kind: "silent"; exit: number };

const ID = /^[a-z0-9][a-z0-9-]*$/;
const HOST = /^(?:\d{1,3}(?:\.\d{1,3}){3}|[a-z0-9]([a-z0-9-]*[a-z0-9])?(?:\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*\.?)$/;

/** Refuses a probe whose fields could not be passed safely or read back unambiguously. */
export function validateProbes(probes: readonly Probe[]): void {
  const ids = new Set<string>();
  for (const probe of probes) {
    if (!ID.test(probe.id) || probe.id === "done") throw new Error(`not a probe id: ${JSON.stringify(probe.id)}`);
    if (ids.has(probe.id)) throw new Error(`two probes are named ${probe.id}`);
    ids.add(probe.id);
    if (probe.kind === "dns") {
      if (!HOST.test(probe.name) || !probe.name.endsWith(".")) throw new Error(`${probe.id}: ${JSON.stringify(probe.name)} is not a fully qualified name with its final dot`);
    } else {
      if (!HOST.test(probe.host)) throw new Error(`${probe.id}: ${JSON.stringify(probe.host)} is not a host`);
      if (!Number.isInteger(probe.port) || probe.port < 1 || probe.port > 65_535) throw new Error(`${probe.id}: ${probe.port} is not a port`);
      if (probe.send.includes("\0")) throw new Error(`${probe.id}: what it sends holds a NUL byte, which no argument can carry`);
    }
  }
  if (probes.length === 0) throw new Error("no probe to run");
}

/**
 * The script a probe container runs: `bash -c <script> tq-probe <limit> (<id> <kind> <host or
 * name> <port> <send>)...`. It waits for the line `go` on its standard input, which kubectl sends
 * once it is attached (for at most PROBE_START_LIMIT_S plus a minute, then it gives up). The probes
 * run at once; each prints one line, then the script prints `tq-probe done`:
 *   tq-probe <id> resolved <first IPv4 address> | tq-probe <id> unresolved
 *   tq-probe <id> answered <base64 of the first ANSWER_LIMIT_BYTES bytes> | tq-probe <id> silent <exit status>
 * The exit status of a silent connection: 3 not connected, 4 not sent, 124 out of time, 0 closed
 * without a byte. Everything a probe sends or names is an argument, never part of the script.
 */
export const PROBE_SCRIPT = `set -u
IFS= read -r -t ${PROBE_START_LIMIT_S + 60} start || exit 5
[ "$start" = go ] || exit 5
limit=$1
shift
probe() {
  local id=$1 kind=$2 target=$3 port=$4 send=$5 out status
  if [ "$kind" = dns ]; then
    out=$(timeout "$limit" getent ahostsv4 "$target" 2>/dev/null | head -n 1)
    if [ -n "$out" ]; then
      printf 'tq-probe %s resolved %s\\n' "$id" "\${out%% *}"
    else
      printf 'tq-probe %s unresolved\\n' "$id"
    fi
  else
    out=$(timeout "$limit" bash -c 'exec 3<>"/dev/tcp/$1/$2" || exit 3; printf "%s" "$3" >&3 || exit 4; head -c ${ANSWER_LIMIT_BYTES} <&3 | base64 -w 0' tq-probe "$target" "$port" "$send" 2>/dev/null)
    status=$?
    if [ -n "$out" ]; then
      printf 'tq-probe %s answered %s\\n' "$id" "$out"
    else
      printf 'tq-probe %s silent %s\\n' "$id" "$status"
    fi
  fi
}
while [ $# -ge 5 ]; do
  probe "$1" "$2" "$3" "$4" "$5" &
  shift 5
done
wait
printf 'tq-probe done\\n'
`;

/** The arguments after the script: the limit, then five per probe. */
export function probeArguments(probes: readonly Probe[], limitSeconds = PROBE_LIMIT_S): string[] {
  validateProbes(probes);
  return [
    String(limitSeconds),
    ...probes.flatMap((probe) => (probe.kind === "dns" ? [probe.id, "dns", probe.name, "0", ""] : [probe.id, "tcp", probe.host, String(probe.port), probe.send])),
  ];
}

/**
 * The results in a probe container's output, by probe id. Strict: every probe exactly once, the
 * closing `tq-probe done`, and no other `tq-probe` line; anything else is an error naming it.
 */
export function parseProbeOutput(output: string, probes: readonly Probe[]): Map<string, ProbeResult> {
  const results = new Map<string, ProbeResult>();
  const known = new Map(probes.map((probe) => [probe.id, probe]));
  let done = false;
  for (const line of output.split("\n").map((l) => l.replace(/\r$/, ""))) {
    if (!line.startsWith("tq-probe ")) continue;
    if (line === "tq-probe done") {
      done = true;
      continue;
    }
    const match = /^tq-probe ([a-z0-9-]+) (resolved|unresolved|answered|silent)(?: (\S+))?$/.exec(line);
    const probe = match === null ? undefined : known.get(match[1]!);
    if (match === null || probe === undefined) throw new Error(`a probe printed ${JSON.stringify(line.slice(0, 120))}, which names no probe of this container`);
    const [, id, kind, value] = match as unknown as [string, string, string, string | undefined];
    if (results.has(id)) throw new Error(`probe ${id} printed twice`);
    const fits = probe.kind === "dns" ? kind === "resolved" || kind === "unresolved" : kind === "answered" || kind === "silent";
    if (!fits) throw new Error(`probe ${id} (${probe.kind}) printed ${kind}`);
    if (kind === "resolved" && value !== undefined && /^\d{1,3}(?:\.\d{1,3}){3}$/.test(value)) results.set(id, { kind, address: value });
    else if (kind === "unresolved" && value === undefined) results.set(id, { kind });
    else if (kind === "answered" && value !== undefined && /^[A-Za-z0-9+/]+=*$/.test(value)) results.set(id, { kind, bytes: Buffer.from(value, "base64").toString("latin1") });
    else if (kind === "silent" && value !== undefined && /^\d{1,3}$/.test(value)) results.set(id, { kind, exit: Number(value) });
    else throw new Error(`probe ${id} printed ${JSON.stringify(line.slice(0, 120))}`);
  }
  const missing = probes.filter((probe) => !results.has(probe.id)).map((probe) => probe.id);
  if (missing.length > 0 || !done) {
    throw new Error(`the probe container ended without ${missing.length > 0 ? `the result of ${missing.join(", ")}` : "its closing line"}`);
  }
  return results;
}

/** An HTTP/1.1 request as bytes on the wire, which asks the server to close the connection after its answer. */
export function httpRequest(method: "GET" | "POST", host: string, path: string, body?: { contentType: string; text: string }): string {
  const lines = [`${method} ${path} HTTP/1.1`, `Host: ${host}`, "Connection: close", "Accept: */*"];
  if (body !== undefined) {
    lines.push(`Content-Type: ${body.contentType}`, `Content-Length: ${Buffer.byteLength(body.text, "utf8")}`);
  }
  // The body goes over the wire as UTF-8; the argument carries it as text, which bash writes as is.
  return `${lines.join("\r\n")}\r\n\r\n${body?.text ?? ""}`;
}

/** An HTTP answer read from Latin-1 bytes: its status and its body (UTF-8, chunks joined). */
export function parseHttpAnswer(bytes: string): { status: number; body: string } | undefined {
  const end = bytes.indexOf("\r\n\r\n");
  const status = /^HTTP\/1\.[01] (\d{3})/.exec(bytes);
  if (status === null) return undefined;
  if (end < 0) return { status: Number(status[1]), body: "" };
  const head = bytes.slice(0, end).toLowerCase();
  let raw = bytes.slice(end + 4);
  if (/^transfer-encoding:\s*chunked\s*$/m.test(head)) {
    let joined = "";
    for (;;) {
      const lineEnd = raw.indexOf("\r\n");
      const size = lineEnd < 0 ? Number.NaN : Number.parseInt(raw.slice(0, lineEnd).split(";")[0]!, 16);
      if (!Number.isFinite(size) || size === 0) break;
      joined += raw.slice(lineEnd + 2, lineEnd + 2 + size);
      raw = raw.slice(lineEnd + 2 + size + 2);
    }
    raw = joined;
  }
  return { status: Number(status[1]), body: Buffer.from(raw, "latin1").toString("utf8") };
}

/** What a probe is expected to see. */
export type Expectation =
  | { open: false }
  | { open: true; status?: number; verdict?: "passed" }
  | { open: true; resolved: true };

/** A probe with what it must see, and the pod it runs from. */
export interface PlannedProbe {
  from: "checker" | "control" | "assistant";
  probe: Probe;
  expect: Expectation;
  /** How the report names it. */
  label: string;
}

/** The release's addresses that the plan names, looked up in the cluster. */
export interface IsolationTargets {
  namespace: string;
  /** The dashboard check's Service, its ClusterIP and port named http, its pod's address and health port. */
  checker: { service: string; clusterIp: string; port: number; podIp: string; healthPort: number };
  /** The OpenTelemetry collector's ClusterIP and OTLP/HTTP port; undefined when the release has none. */
  collector?: { clusterIp: string; port: number };
  /** The `kubernetes` Service's ClusterIP and port, and the API server's endpoints. */
  apiServer: { clusterIp: string; port: number; endpoints: { host: string; port: number }[] };
  /** The body of the assistant's check: the sandbox's shared request with its `correct` fixture page. */
  checkBody: string;
}

/**
 * The probes of the isolation check. Every target the dashboard check's pod must not reach is
 * probed the same way from the identity pod, where it must answer: without that control a silent
 * probe could mean a target that is down or a probe that does not work.
 */
export function isolationPlan(targets: IsolationTargets): PlannedProbe[] {
  const get = (host: string) => httpRequest("GET", host, "/");
  const egress: { id: string; label: string; probe: (id: string) => Probe; status?: number }[] = [
    { id: "dns", label: `DNS ${CLUSTER_NAME}`, probe: (id) => ({ kind: "dns", id, name: CLUSTER_NAME }) },
    {
      id: "api-service",
      label: `the API server's Service ${targets.apiServer.clusterIp}:${targets.apiServer.port}`,
      probe: (id) => ({ kind: "tcp", id, host: targets.apiServer.clusterIp, port: targets.apiServer.port, send: get(targets.apiServer.clusterIp) }),
    },
    ...targets.apiServer.endpoints.map((endpoint, index) => ({
      id: `api-endpoint-${index}`,
      label: `the API server at ${endpoint.host}:${endpoint.port}`,
      probe: (id: string): Probe => ({ kind: "tcp", id, host: endpoint.host, port: endpoint.port, send: get(endpoint.host) }),
    })),
    ...(targets.collector === undefined
      ? []
      : [
          {
            id: "collector",
            label: `the OpenTelemetry collector ${targets.collector.clusterIp}:${targets.collector.port}`,
            probe: (id: string): Probe => ({ kind: "tcp", id, host: targets.collector!.clusterIp, port: targets.collector!.port, send: get(targets.collector!.clusterIp) }),
          },
        ]),
    {
      id: "outside",
      label: `${OUTSIDE.host}:${OUTSIDE.port} outside the cluster`,
      probe: (id) => ({ kind: "tcp", id, host: OUTSIDE.host, port: OUTSIDE.port, send: get(OUTSIDE.host) }),
    },
  ];
  const { checker } = targets;
  const checkHost = `${checker.service}.${targets.namespace}.svc.cluster.local`;
  const checkRequest = httpRequest("POST", checkHost, CHECK_ROUTE, { contentType: "application/json", text: targets.checkBody });
  return [
    {
      from: "checker",
      probe: { kind: "tcp", id: "loopback", host: "127.0.0.1", port: checker.healthPort, send: httpRequest("GET", "127.0.0.1", READY_PATH) },
      expect: { open: true, status: 200 },
      label: `its own ${READY_PATH} on 127.0.0.1:${checker.healthPort}`,
    },
    ...egress.map((target) => ({ from: "checker" as const, probe: target.probe(target.id), expect: { open: false } as const, label: target.label })),
    ...egress.map((target) => ({
      from: "control" as const,
      probe: target.probe(target.id),
      expect: target.id === "dns" ? ({ open: true, resolved: true } as const) : ({ open: true } as const),
      label: target.label,
    })),
    {
      from: "control",
      probe: { kind: "tcp", id: "checker-service", host: checker.clusterIp, port: checker.port, send: httpRequest("POST", checkHost, CHECK_ROUTE, { contentType: "application/json", text: "{}" }) },
      expect: { open: false },
      label: `the dashboard check's Service ${checker.clusterIp}:${checker.port}`,
    },
    {
      from: "control",
      probe: { kind: "tcp", id: "checker-pod", host: checker.podIp, port: checker.port, send: httpRequest("POST", checker.podIp, CHECK_ROUTE, { contentType: "application/json", text: "{}" }) },
      expect: { open: false },
      label: `the dashboard check's pod ${checker.podIp}:${checker.port}`,
    },
    {
      from: "assistant",
      probe: { kind: "tcp", id: "check", host: checkHost, port: checker.port, send: checkRequest },
      expect: { open: true, status: 200, verdict: "passed" },
      label: `POST ${CHECK_ROUTE} on ${checkHost}:${checker.port} with the sandbox's correct fixture page`,
    },
  ];
}

/** Why `result` is not what `planned` expects, or undefined when it is. */
export function probeProblem(planned: PlannedProbe, result: ProbeResult | undefined): string | undefined {
  const where = `from the ${planned.from === "checker" ? "dashboard check's" : planned.from === "control" ? "identity service's" : "assistant's"} pod, ${planned.label}`;
  if (result === undefined) return `${where}: no result`;
  const seen = describe(result);
  const { expect } = planned;
  if (!expect.open) {
    return result.kind === "unresolved" || result.kind === "silent" ? undefined : `${where}: ${seen}, expected no answer`;
  }
  if ("resolved" in expect) return result.kind === "resolved" ? undefined : `${where}: ${seen}, expected the name to resolve`;
  if (result.kind !== "answered") return `${where}: ${seen}, expected an answer`;
  if (expect.status === undefined) return undefined;
  const answer = parseHttpAnswer(result.bytes);
  if (answer === undefined || answer.status !== expect.status) return `${where}: ${seen}, expected HTTP ${expect.status}`;
  if (expect.verdict !== undefined) {
    let verdict: unknown;
    try {
      verdict = (JSON.parse(answer.body) as { verdict?: unknown }).verdict;
    } catch {
      verdict = undefined;
    }
    if (verdict !== expect.verdict) return `${where}: HTTP ${answer.status} with ${JSON.stringify(answer.body.slice(0, 300))}, expected the verdict ${expect.verdict}`;
  }
  return undefined;
}

/** A short account of a result for the report: never the whole answer. */
export function describe(result: ProbeResult): string {
  switch (result.kind) {
    case "resolved":
      return `resolved to ${result.address}`;
    case "unresolved":
      return "did not resolve";
    case "silent":
      return `no answer (${result.exit === 3 ? "not connected" : result.exit === 124 ? "out of time" : result.exit === 0 ? "closed without a byte" : `exit ${result.exit}`})`;
    case "answered": {
      const answer = parseHttpAnswer(result.bytes);
      return answer === undefined ? `answered ${result.bytes.length} byte(s)` : `answered HTTP ${answer.status}`;
    }
  }
}

/**
 * The first (by name) of the pods of `component` that are running and ready, not being deleted;
 * throws when there is none.
 */
export function readyPodOf(podList: { items?: unknown[] }, component: string): { name: string; ip: string } {
  const pods = (podList.items ?? []).filter((item): item is Record<string, unknown> => typeof item === "object" && item !== null);
  const ready = pods.filter((pod) => {
    const metadata = (pod.metadata ?? {}) as { labels?: Record<string, string>; deletionTimestamp?: string };
    const status = (pod.status ?? {}) as { phase?: string; podIP?: string; conditions?: { type?: string; status?: string }[] };
    return (
      metadata.labels?.["app.kubernetes.io/component"] === component &&
      metadata.deletionTimestamp === undefined &&
      status.phase === "Running" &&
      typeof status.podIP === "string" &&
      (status.conditions ?? []).some((condition) => condition.type === "Ready" && condition.status === "True")
    );
  });
  const found = ready
    .map((pod) => ({ name: String((pod.metadata as { name?: unknown }).name), ip: String((pod.status as { podIP?: unknown }).podIP) }))
    .toSorted((a, b) => a.name.localeCompare(b.name));
  if (found.length === 0) throw new Error(`no pod of ${component} is running and ready`);
  return found[0]!;
}

/** The number of the container port `portName` of the Deployment of `component` in the release. */
export function containerPortOf(objects: K8sObject[], component: string, portName: string): number {
  const deployments = objects.filter((o) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === component);
  if (deployments.length !== 1) throw new Error(`the release should have one Deployment of ${component}, found ${deployments.length}`);
  const template = (deployments[0]!.spec?.template ?? {}) as { spec?: { containers?: { ports?: { name?: string; containerPort?: number }[] }[] } };
  const ports = (template.spec?.containers ?? []).flatMap((container) => container.ports ?? []).filter((port) => port.name === portName);
  if (ports.length !== 1 || typeof ports[0]!.containerPort !== "number") throw new Error(`the Deployment of ${component} should have one container port named ${portName}, found ${ports.length}`);
  return ports[0]!.containerPort;
}

/** The sandbox's shared request (frontend/sandbox/fixtures/request.json) with the `correct` fixture page. */
export function checkBodyOf(repoRoot: string): string {
  const fixtures = join(repoRoot, "frontend", "sandbox", "fixtures");
  const cases = JSON.parse(readFileSync(join(fixtures, "cases.json"), "utf8")) as { request: string; cases: { name: string; page: string; expect: { verdict?: string } }[] };
  const correct = cases.cases.find((fixture) => fixture.name === "correct");
  if (correct === undefined || correct.expect.verdict !== "passed") throw new Error("frontend/sandbox/fixtures/cases.json has no `correct` page that passes");
  const request = JSON.parse(readFileSync(join(fixtures, cases.request), "utf8")) as Record<string, unknown>;
  return JSON.stringify({ ...request, page: readFileSync(join(fixtures, correct.page), "utf8") });
}

/** How the check reaches the cluster; tests replace it. */
export interface IsolationDeps {
  /** Runs `probes` in a new ephemeral container of `pod` and returns what they saw. */
  probe: (pod: string, probes: Probe[]) => Promise<Map<string, ProbeResult>>;
  log: (line: string) => void;
}

/** The pods the probes run from. */
export interface IsolationPods {
  checker: string;
  control: string;
  assistant: string;
}

/**
 * Runs the plan from the three pods and returns the report line; throws with every probe that saw
 * something else than expected.
 */
export async function checkIsolation(deps: IsolationDeps, pods: IsolationPods, targets: IsolationTargets): Promise<string> {
  const plan = isolationPlan(targets);
  const problems: string[] = [];
  const seen = new Map<PlannedProbe, ProbeResult | undefined>();
  for (const from of ["checker", "control", "assistant"] as const) {
    const planned = plan.filter((entry) => entry.from === from);
    const results = await deps.probe(pods[from], planned.map((entry) => entry.probe));
    for (const entry of planned) {
      const result = results.get(entry.probe.id);
      seen.set(entry, result);
      const problem = probeProblem(entry, result);
      if (problem !== undefined) problems.push(problem);
      deps.log(`${from} ${entry.probe.id}: ${result === undefined ? "no result" : describe(result)}`);
    }
  }
  if (problems.length > 0) throw new Error(problems.join("; "));
  const account = (from: PlannedProbe["from"]) =>
    plan
      .filter((entry) => entry.from === from)
      .map((entry) => `${entry.label}: ${describe(seen.get(entry)!)}`)
      .join(", ");
  return (
    `from the dashboard check's pod ${pods.checker}: ${account("checker")}; ` +
    `from the identity service's pod ${pods.control}: ${account("control")}; ` +
    `from the assistant's pod ${pods.assistant}: ${account("assistant")} with the verdict passed`
  );
}

/** Where the check runs: the smoke cluster and its log. */
export interface IsolationContext {
  kubeconfig: string;
  kubectl: string;
  log: RunLog;
  repoRoot: string;
}

/**
 * The `kubectl debug` command that runs `probes` in a new ephemeral container `name` of `pod`:
 * the restricted profile, the probe image from the node, attached (`--stdin`) until it ends.
 */
export function probeCommand(kubectl: string, namespace: string, pod: string, name: string, probes: readonly Probe[]): string[] {
  if (!ID.test(name) || !ID.test(pod)) throw new Error(`not a container or pod name: ${JSON.stringify(name)}, ${JSON.stringify(pod)}`);
  return [
    kubectl, "debug", "--namespace", namespace, `pod/${pod}`,
    `--container=${name}`, `--image=${PROBE_IMAGE}`, "--image-pull-policy=Never", "--profile=restricted",
    "--stdin", "--quiet", `--pod-running-timeout=${PROBE_START_LIMIT_S}s`,
    "--", "bash", "-c", PROBE_SCRIPT, "tq-probe", ...probeArguments(probes),
  ];
}

type Exec = (cmd: string[], options: ExecOptions) => Promise<ExecResult>;

/**
 * Runs `probes` in a new ephemeral container `name` of `pod` and returns what they saw. `name`
 * must be new in the pod: an ephemeral container can be neither removed nor reused. The command
 * line holds the requests the probes send (the fixture page among them), so only a summary of it
 * reaches the log; the output is the probes' result lines, which are parsed, not logged.
 */
export async function probeInPod(
  ctx: Pick<IsolationContext, "kubeconfig" | "kubectl" | "log">,
  namespace: string,
  pod: string,
  name: string,
  probes: Probe[],
  commands: { exec: Exec } = { exec },
): Promise<Map<string, ProbeResult>> {
  const cmd = probeCommand(ctx.kubectl, namespace, pod, name, probes);
  ctx.log.line(`$ kubectl debug --namespace ${namespace} pod/${pod} --container=${name} --image=${PROBE_IMAGE} --profile=restricted --stdin -- bash -c <probe script> (${probes.map((probe) => probe.id).join(", ")})`);
  const result = await commands.exec(cmd, { env: { KUBECONFIG: ctx.kubeconfig }, stdin: "go\n", timeoutMs: PROBE_CONTAINER_LIMIT_MS });
  ctx.log.block(result.stderr);
  const how = result.timedOut ? `timed out after ${PROBE_CONTAINER_LIMIT_MS / 1000} s` : `exited with ${result.code}`;
  try {
    return parseProbeOutput(result.stdout, probes);
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    const tail = result.stderr.trim().split("\n").slice(-5).join("\n");
    throw new Error(`probe container ${name} in pod ${pod}: kubectl debug ${how}; ${reason}${tail === "" ? "" : `:\n${tail}`}`);
  }
}

/**
 * The isolation check against the release in `namespace` (see the top of this file). Returns the
 * report line; throws when the release has no dashboard check, when a pod or address cannot be
 * found, or when a probe sees something else than expected.
 */
export async function runIsolationCheck(ctx: IsolationContext, namespace: string, objects: K8sObject[]): Promise<string> {
  const env = { KUBECONFIG: ctx.kubeconfig };
  const json = async (args: string[]) => JSON.parse((await run(ctx.log, [ctx.kubectl, ...args, "--output", "json"], { env, timeoutMs: 60_000, quiet: true })).stdout) as unknown;
  // Pods and Services: their JSON holds environment values, so it stays out of the log (quiet).
  const clusterIp = async (service: string) => {
    const ip = ((await json(["get", "service", service, "--namespace", namespace])) as { spec?: { clusterIP?: string } }).spec?.clusterIP ?? "";
    if (!/^\d{1,3}(?:\.\d{1,3}){3}$/.test(ip)) throw new Error(`Service ${service} has no IPv4 ClusterIP (${JSON.stringify(ip)})`);
    return ip;
  };

  const checker = servicePortOf(objects, DASHBOARD_CHECK, "http");
  const healthPort = containerPortOf(objects, DASHBOARD_CHECK, HEALTH_PORT_NAME);
  const hasCollector = objects.some((o) => o.kind === "Service" && o.metadata?.labels?.["app.kubernetes.io/component"] === "otel-collector");
  const collector = hasCollector ? servicePortOf(objects, "otel-collector", "otlp-http") : undefined;

  const pods = (await json(["get", "pods", "--namespace", namespace])) as { items?: unknown[] };
  const checkerPod = readyPodOf(pods, DASHBOARD_CHECK);
  const controlPod = readyPodOf(pods, "identity");
  const assistantPod = readyPodOf(pods, "assistant");

  const kubernetes = (await json(["get", "service", "kubernetes", "--namespace", "default"])) as { spec?: { clusterIP?: string; ports?: { name?: string; port?: number }[] } };
  const apiPort = kubernetes.spec?.ports?.find((port) => port.name === "https")?.port;
  const slices = (await json(["get", "endpointslices", "--namespace", "default", "--selector", "kubernetes.io/service-name=kubernetes"])) as {
    items?: { endpoints?: { addresses?: string[] }[]; ports?: { name?: string; port?: number }[] }[];
  };
  const endpoints = (slices.items ?? []).flatMap((slice) => {
    const port = slice.ports?.find((candidate) => candidate.name === "https")?.port;
    return port === undefined ? [] : (slice.endpoints ?? []).flatMap((endpoint) => (endpoint.addresses ?? []).map((host) => ({ host, port })));
  });
  if (kubernetes.spec?.clusterIP === undefined || apiPort === undefined || endpoints.length === 0) {
    throw new Error("cannot find the API server's Service address and endpoints in the default namespace");
  }

  const targets: IsolationTargets = {
    namespace,
    checker: { service: checker.service, clusterIp: await clusterIp(checker.service), port: checker.port, podIp: checkerPod.ip, healthPort },
    ...(collector === undefined ? {} : { collector: { clusterIp: await clusterIp(collector.service), port: collector.port } }),
    apiServer: { clusterIp: kubernetes.spec.clusterIP, port: apiPort, endpoints },
    checkBody: checkBodyOf(ctx.repoRoot),
  };
  let containers = 0;
  return checkIsolation(
    {
      probe: (pod, probes) => probeInPod(ctx, namespace, pod, `tq-probe-${++containers}`, probes),
      log: (line) => ctx.log.line(line),
    },
    { checker: checkerPod.name, control: controlPod.name, assistant: assistantPod.name },
    targets,
  );
}
