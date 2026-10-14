// Tests of the nightly diagnostics (Laysi/trustiot-v2#410), with no cluster: a fake kubectl plays
// a cluster in which three things are wrong at once (a service that is killed for its memory, a
// service whose image cannot be pulled, a Postgres Cluster that does not come up) and in which
// every Pod, Cluster and event also carries values the report must never show. The values are made
// at run time, so no credential-looking literal is in this file. Runs with `bun test scripts/nightly`.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { probeMessage, withoutUrlSecrets } from "../../deploy/chart/tests/e2e/install-evidence.ts";
import { formatDiagnostics, gatherDiagnostics, OBJECT_KINDS, type Command } from "./diagnostics.ts";
import { failureIssueBody, type RunInfo } from "./notify.ts";
import { renderMarkdown, type Report } from "./report.ts";

const canary = (label: string): string => `${label}-${crypto.randomUUID()}${crypto.randomUUID()}`;

// What the report must never contain. Each stands for something a Pod, a container or an event can
// carry that is not a status field.
const ENV_VALUE = canary("env");
const ANNOTATION = canary("annotation");
const TERMINATION_MESSAGE = canary("termination");
const LOG_LINE = canary("log");
const SECRET_DATA = canary("secret");
const PROBE_OUTPUT = canary("probe");
const URL_TOKEN = canary("token");
const URL_USER = canary("user");
const CLUSTER_SPEC_VALUE = canary("cluster-spec");
const ALL_CANARIES = [ENV_VALUE, ANNOTATION, TERMINATION_MESSAGE, LOG_LINE, SECRET_DATA, PROBE_OUTPUT, URL_TOKEN, URL_USER, CLUSTER_SPEC_VALUE];

const NAMESPACE = "nightly-saas";
const owner = (kind: string, name: string) => [{ kind, name }];

const identityPod = {
  metadata: { name: "trustiot-identity-7d9f-abcde", namespace: NAMESPACE, ownerReferences: owner("ReplicaSet", "trustiot-identity-7d9f"), annotations: { note: ANNOTATION } },
  spec: {
    containers: [
      {
        name: "identity",
        image: "localhost/trustiot-e2e/identity:nightly",
        env: [
          { name: "PLAIN", value: ENV_VALUE },
          { name: "FROM_SECRET", valueFrom: { secretKeyRef: { name: "trustiot-identity-key", key: "pem" } } },
        ],
      },
    ],
  },
  status: {
    phase: "Running",
    conditions: [
      { type: "Ready", status: "False", reason: "ContainersNotReady", message: "containers with unready status: [identity]" },
      { type: "ContainersReady", status: "False", reason: "ContainersNotReady", message: "containers with unready status: [identity]" },
    ],
    containerStatuses: [
      {
        name: "identity",
        ready: false,
        restartCount: 4,
        state: { waiting: { reason: "CrashLoopBackOff", message: "back-off 1m20s restarting failed container=identity pod=trustiot-identity-7d9f-abcde" } },
        lastState: { terminated: { exitCode: 137, reason: "OOMKilled", message: TERMINATION_MESSAGE } },
      },
    ],
  },
};

const assetsPod = {
  metadata: { name: "trustiot-assets-5c8d-fghij", namespace: NAMESPACE, ownerReferences: owner("ReplicaSet", "trustiot-assets-5c8d") },
  spec: { containers: [{ name: "assets", env: [{ name: "PLAIN", value: ENV_VALUE }] }], initContainers: [{ name: "wait-for-migration", env: [{ name: "PLAIN", value: ENV_VALUE }] }] },
  status: {
    phase: "Pending",
    conditions: [{ type: "PodScheduled", status: "True" }, { type: "Ready", status: "False", reason: "ContainersNotReady", message: "containers with unready status: [assets]" }],
    initContainerStatuses: [{ name: "wait-for-migration", ready: true, restartCount: 0, state: { terminated: { exitCode: 0, reason: "Completed", message: TERMINATION_MESSAGE } } }],
    containerStatuses: [
      {
        name: "assets",
        ready: false,
        restartCount: 0,
        state: {
          waiting: {
            reason: "ErrImagePull",
            message: `failed to pull and unpack image "registry.example.test/trustiot/assets:nightly": failed to resolve reference: unexpected status from HEAD request to https://${URL_USER}:${URL_TOKEN}@registry.example.test/v2/trustiot/assets/manifests/nightly?token=${URL_TOKEN}: 401 Unauthorized`,
          },
        },
      },
    ],
  },
};

const clusterJson = {
  items: [
    {
      metadata: { name: "trustiot-postgres", namespace: NAMESPACE },
      spec: { instances: 1, bootstrap: { initdb: { database: "trustiot", secret: { name: "trustiot-postgres-owner" } } }, env: [{ name: "X", value: CLUSTER_SPEC_VALUE }] },
      status: {
        phase: "Setting up primary",
        phaseReason: "Creating primary instance trustiot-postgres-1",
        instances: 1,
        readyInstances: 0,
        currentPrimary: "trustiot-postgres-1",
        instancesStatus: { failed: ["trustiot-postgres-1"] },
        certificates: { serverCASecret: SECRET_DATA },
        conditions: [{ type: "Ready", status: "False", reason: "ClusterIsNotReady", message: "Cluster Is Not Ready" }],
      },
    },
  ],
};

const gatewayJson = {
  items: [
    {
      metadata: { name: "trustiot-ingress" },
      status: {
        conditions: [{ type: "Programmed", status: "False", reason: "Pending", message: "Resource not yet programmed" }],
        listeners: [{ name: "https", attachedRoutes: 0, conditions: [{ type: "ResolvedRefs", status: "False", reason: "InvalidCertificateRef", message: "secret trustiot-ingress-tls not found" }] }],
      },
    },
  ],
};

const eventsJson = {
  items: [
    { type: "Warning", reason: "BackOff", involvedObject: { kind: "Pod", name: "trustiot-identity-7d9f-abcde" }, message: "Back-off restarting failed container identity in pod trustiot-identity-7d9f-abcde", lastTimestamp: "2026-10-15T01:12:40Z", count: 5, metadata: { annotations: { note: ANNOTATION } } },
    { type: "Warning", reason: "Unhealthy", involvedObject: { kind: "Pod", name: "trustiot-identity-7d9f-abcde" }, message: `Readiness probe failed: ${PROBE_OUTPUT}`, lastTimestamp: "2026-10-15T01:12:30Z" },
    { type: "Warning", reason: "Unhealthy", involvedObject: { kind: "Pod", name: "trustiot-identity-7d9f-abcde" }, message: "Readiness probe failed: HTTP probe failed with statuscode: 503", lastTimestamp: "2026-10-15T01:12:20Z" },
    { type: "Warning", reason: "Failed", involvedObject: { kind: "Pod", name: "trustiot-assets-5c8d-fghij" }, message: `Failed to pull image "registry.example.test/trustiot/assets:nightly": pull access denied, repository does not exist or may require authorization: server message: insufficient_scope: authorization failed (see https://registry.example.test/token?service=x&scope=y&token=${URL_TOKEN})`, lastTimestamp: "2026-10-15T01:12:10Z" },
    { type: "Normal", reason: "Scheduled", involvedObject: { kind: "Pod", name: "trustiot-identity-7d9f-abcde" }, message: `Successfully assigned ${NAMESPACE}/trustiot-identity-7d9f-abcde to k3d-nightly-server-0`, lastTimestamp: "2026-10-15T01:11:00Z" },
  ],
};

const operatorPod = {
  metadata: { name: "cnpg-controller-manager-6c7d-xyz12", namespace: "cnpg-system" },
  spec: { containers: [{ name: "manager", env: [{ name: "WEBHOOK_TOKEN", value: ENV_VALUE }] }] },
  status: { phase: "Running", containerStatuses: [{ name: "manager", ready: true, restartCount: 0, state: { running: {} } }] },
};

const JSON_BY_KEY: Record<string, unknown> = {
  nodes: { items: [{ metadata: { name: "k3d-nightly-server-0" }, status: { conditions: [{ type: "MemoryPressure", status: "False", reason: "KubeletHasSufficientMemory", message: "kubelet has sufficient memory available" }, { type: "Ready", status: "True", reason: "KubeletReady" }], nodeInfo: { machineID: SECRET_DATA } } }] },
  deployments: { items: [{ metadata: { name: "trustiot-identity", generation: 1 }, spec: { replicas: 1, template: { spec: { containers: [{ env: [{ name: "PLAIN", value: ENV_VALUE }] }] } } }, status: { observedGeneration: 1, replicas: 1, updatedReplicas: 1, conditions: [{ type: "Available", status: "False", reason: "MinimumReplicasUnavailable", message: "Deployment does not have minimum availability." }] } }] },
  statefulsets: { items: [{ metadata: { name: "trustiot-clickhouse" }, spec: { replicas: 1 }, status: { replicas: 1, readyReplicas: 0 } }] },
  daemonsets: { items: [] },
  replicasets: { items: [{ metadata: { name: "trustiot-identity-7d9f", ownerReferences: owner("Deployment", "trustiot-identity") }, spec: { replicas: 1 }, status: { replicas: 1, readyReplicas: 0 } }] },
  jobs: { items: [{ metadata: { name: "trustiot-migrate" }, status: { active: 0, succeeded: 1, failed: 0, conditions: [{ type: "Complete", status: "True" }] } }] },
  pods: { items: [identityPod, assetsPod] },
  persistentvolumeclaims: { items: [{ metadata: { name: "trustiot-postgres-1" }, status: { phase: "Bound" } }] },
  "clusters.postgresql.cnpg.io": clusterJson,
  "gateways.gateway.networking.k8s.io": gatewayJson,
  events: eventsJson,
  "pods:all": { items: [identityPod, assetsPod, operatorPod, { metadata: { name: "ztunnel-abcde", namespace: "istio-system" }, status: { phase: "Running", containerStatuses: [{ name: "istio-proxy", ready: false, restartCount: 2, state: { waiting: { reason: "CrashLoopBackOff" } }, lastState: { terminated: { exitCode: 1, reason: "Error", message: TERMINATION_MESSAGE } } }] } }] },
  "events:all": {
    items: [
      ...eventsJson.items.map((event) => ({ ...event, metadata: { namespace: NAMESPACE } })),
      { type: "Warning", reason: "FailedMount", involvedObject: { kind: "Pod", name: "cnpg-controller-manager-6c7d-xyz12", namespace: "cnpg-system" }, metadata: { namespace: "cnpg-system" }, message: 'MountVolume.SetUp failed for volume "webhook-certificates" : secret "cnpg-webhook-cert" not found', lastTimestamp: "2026-10-15T01:00:00Z" },
      { type: "Normal", reason: "Pulled", involvedObject: { kind: "Pod", name: "ztunnel-abcde", namespace: "istio-system" }, metadata: { namespace: "istio-system" }, message: "Container image already present on machine", lastTimestamp: "2026-10-15T01:00:01Z" },
    ],
  },
};

const OBJECT_NAMES = ["pod/trustiot-identity-7d9f-abcde", "secret/trustiot-identity-key", "secret/trustiot-postgres-owner", "configmap/kube-root-ca.crt"].join("\n");

interface FakeCluster {
  calls: string[][];
  kubectl: Command;
}

/** A kubectl that answers `get` of the kinds above and records every call; any other verb throws. */
function fakeCluster(overrides: Record<string, unknown | Error> = {}): FakeCluster {
  const calls: string[][] = [];
  const answers = { ...JSON_BY_KEY, ...overrides };
  const kubectl: Command = async (args) => {
    calls.push(args);
    if (args[0] !== "get") throw new Error(`the diagnostics must only run kubectl get, not ${args[0]}`);
    if (args.includes("name")) return OBJECT_NAMES;
    const key = `${args[1]}${args.includes("--all-namespaces") ? ":all" : ""}`;
    const answer = answers[key];
    if (answer instanceof Error) throw answer;
    if (answer === undefined) throw new Error(`no such kind: ${key}`);
    return JSON.stringify(answer);
  };
  return { calls, kubectl };
}

const helmList = async () => "NAME\tNAMESPACE\tREVISION\tSTATUS\nrelease\tnightly-saas\t1\tfailed\n";

describe("the diagnostics of a failed shape", () => {
  let text = "";
  let calls: string[][] = [];
  test("are gathered", async () => {
    const cluster = fakeCluster();
    text = await gatherDiagnostics(NAMESPACE, { kubectl: cluster.kubectl, helmList });
    calls = cluster.calls;
    expect(text.length).toBeGreaterThan(0);
  });

  test("never contain an environment value, an annotation, a termination message, secret data or a spec value", () => {
    for (const value of ALL_CANARIES) expect(text).not.toContain(value);
    for (const word of ["secretKeyRef", "PLAIN", "FROM_SECRET", "WEBHOOK_TOKEN", "annotations", "machineID", "bootstrap", "initdb"]) expect(text).not.toContain(word);
  });

  test("run only `kubectl get`: no logs, no describe, no Secret read, no jsonpath", () => {
    expect(calls.length).toBeGreaterThan(10);
    for (const args of calls) {
      expect(args[0]).toBe("get");
      expect(args.join(" ")).not.toMatch(/\b(logs|describe|exec|port-forward|jsonpath|go-template|custom-columns)\b/);
      // A Secret is only ever listed by name, together with the other kinds.
      if (args.join(",").includes("secret")) expect(args).toEqual(["get", OBJECT_KINDS, "--namespace", NAMESPACE, "--output", "name"]);
    }
    expect(text).toContain("secret/trustiot-identity-key");
  });

  test("show which component is not ready and why: Deployment, ReplicaSet and Pod conditions", () => {
    expect(text).toContain("Deployment/trustiot-identity: desired=1 replicas=1 updated=1 ready=0 available=0 generation=1/observed=1");
    expect(text).toContain("Available=False (MinimumReplicasUnavailable: Deployment does not have minimum availability.)");
    expect(text).toContain("ReplicaSet/trustiot-identity-7d9f: owner=Deployment/trustiot-identity desired=1 replicas=1 ready=0");
    expect(text).toContain("Pod/trustiot-identity-7d9f-abcde: owner=ReplicaSet/trustiot-identity-7d9f phase=Running");
    expect(text).toContain("Ready=False (ContainersNotReady: containers with unready status: [identity])");
  });

  test("show the restart count and the last termination: reason and exit code", () => {
    expect(text).toContain("identity: waiting (CrashLoopBackOff: back-off 1m20s restarting failed container=identity pod=trustiot-identity-7d9f-abcde), ready=false, restarts=4; last exit 137, OOMKilled");
  });

  test("show an image that cannot be pulled, without the URL's user info and query", () => {
    expect(text).toContain("assets: waiting (ErrImagePull: failed to pull and unpack image");
    expect(text).toContain("https://(user info removed)@registry.example.test/v2/trustiot/assets/manifests/nightly?(query removed)");
    expect(text).toContain("Warning Failed Pod/trustiot-assets-5c8d-fghij: Failed to pull image");
    expect(text).toContain("https://registry.example.test/token?(query removed)");
    expect(text).toContain("wait-for-migration: terminated (exit 0, Completed), ready=true, restarts=0");
  });

  test("show the Postgres Cluster's phase, reason, instances and conditions, not its spec", () => {
    expect(text).toContain("Cluster/trustiot-postgres: phase=Setting up primary reason=Creating primary instance trustiot-postgres-1 instances=1 ready=0 primary=trustiot-postgres-1");
    expect(text).toContain("instancesStatus: failed=[trustiot-postgres-1]");
    expect(text).toContain("Ready=False (ClusterIsNotReady: Cluster Is Not Ready)");
  });

  test("show the Gateway and its listeners, workloads and volumes", () => {
    expect(text).toContain("Gateway/trustiot-ingress: conditions: Programmed=False (Pending: Resource not yet programmed)");
    expect(text).toContain("listener https: attachedRoutes=0 conditions: ResolvedRefs=False (InvalidCertificateRef: secret trustiot-ingress-tls not found)");
    expect(text).toContain("StatefulSet/trustiot-clickhouse: desired=1 replicas=1 ready=0");
    expect(text).toContain("Job/trustiot-migrate: active=0 succeeded=1 failed=0 conditions: Complete=True");
    expect(text).toContain("PersistentVolumeClaim/trustiot-postgres-1: phase=Bound");
    expect(text).toContain("Node/k3d-nightly-server-0: conditions: MemoryPressure=False (KubeletHasSufficientMemory:");
  });

  test("show the platform's pods and only its warning events, by namespace", () => {
    expect(text).toContain("namespace cnpg-system:\nPod/cnpg-controller-manager-6c7d-xyz12: phase=Running");
    expect(text).toContain("namespace istio-system:\nPod/ztunnel-abcde: phase=Running\n  istio-proxy: waiting (CrashLoopBackOff), ready=false, restarts=2; last exit 1, Error");
    expect(text).toContain("Warning FailedMount Pod/cnpg-system/cnpg-controller-manager-6c7d-xyz12");
    expect(text).not.toContain("Container image already present on machine");
    // The smoke namespace's own events are in its own section, once.
    expect(text.split("Back-off restarting failed container identity").length - 1).toBe(1);
  });

  test("list the events oldest first, and keep a probe's status code but not a command probe's output", () => {
    const events = text.slice(text.lastIndexOf("### Events"));
    const order = ["Scheduled", "Failed Pod/trustiot-assets", "HTTP probe failed with statuscode: 503", "Readiness probe failed: (probe output not shown)", "BackOff"].map((part) => events.indexOf(part));
    expect(order.every((index) => index >= 0)).toBe(true);
    expect(order).toEqual([...order].sort((a, b) => a - b));
    expect(events).toContain("BackOff Pod/trustiot-identity-7d9f-abcde x5: Back-off restarting failed container identity");
  });
});

describe("when something cannot be read", () => {
  test("the section says so and the others stay", async () => {
    const cluster = fakeCluster({ events: new Error("the server was unreachable"), "clusters.postgresql.cnpg.io": new Error('the server doesn\'t have a resource type "clusters"') });
    const text = await gatherDiagnostics(NAMESPACE, { kubectl: cluster.kubectl, helmList: async () => Promise.reject(new Error("helm list failed")) });
    expect(text).toContain("### Events\n(not available: the server was unreachable)");
    expect(text).toContain("### CloudNativePG Clusters\n(not available: the server doesn't have a resource type \"clusters\")");
    expect(text).toContain("### helm list\n(not available: helm list failed)");
    expect(text).toContain("Pod/trustiot-identity-7d9f-abcde");
  });

  test("output that is not JSON is reported in its section", async () => {
    const text = await gatherDiagnostics(NAMESPACE, { kubectl: async (args) => (args.includes("name") ? "" : "<html>not json"), helmList });
    expect(text).toContain("### Pods\n(not available:");
  });

  test("an empty cluster gives (none) and tolerates objects without status", () => {
    const empty = { items: [] };
    const text = formatDiagnostics(NAMESPACE, {
      helmList: "", objectNames: "", nodes: {}, deployments: { items: [{}] }, statefulSets: null, daemonSets: empty, replicaSets: empty, jobs: empty,
      pods: { items: [{}] }, volumeClaims: empty, postgresClusters: { items: [{}] }, gateways: { items: [{}] }, events: empty, allPods: empty, allEvents: empty,
    });
    expect(text).toContain("### Events\n(none)");
    expect(text).toContain("Deployment/(no name): desired=1");
    expect(text).toContain("Cluster/(no name): phase=unknown");
  });

  test("a long section keeps its last lines", () => {
    const items = Array.from({ length: 300 }, (_, index) => ({ metadata: { name: `claim-${index}` }, status: { phase: "Bound" } }));
    const text = formatDiagnostics(NAMESPACE, {
      helmList: "", objectNames: "", nodes: {}, deployments: {}, statefulSets: {}, daemonSets: {}, replicaSets: {}, jobs: {}, pods: {}, volumeClaims: { items }, postgresClusters: {}, gateways: {}, events: {}, allPods: {}, allEvents: {},
    });
    expect(text).toContain("(180 earlier lines not shown)");
    expect(text).toContain("claim-299");
    expect(text).not.toContain("claim-0:");
  });
});

describe("free text", () => {
  test("a URL loses its user info and query string, and nothing else", () => {
    expect(withoutUrlSecrets("GET https://user:pw@example.test:8443/a/b?x=1&y=2#frag failed")).toBe("GET https://(user info removed)@example.test:8443/a/b?(query removed) failed");
    expect(withoutUrlSecrets('Get "http://10.42.0.5:8080/healthz": dial tcp 10.42.0.5:8080: connect: connection refused')).toBe('Get "http://10.42.0.5:8080/healthz": dial tcp 10.42.0.5:8080: connect: connection refused');
    expect(withoutUrlSecrets("what? no url here")).toBe("what? no url here");
  });

  test("a failed probe's reason is shown only as a fixed wording of what the kubelet or the network wrote", () => {
    expect(probeMessage("Liveness probe failed: HTTP probe failed with statuscode: 500")).toBe("Liveness probe failed: HTTP probe failed with statuscode: 500");
    expect(probeMessage('Readiness probe failed: Get "http://10.42.0.5:8080/healthz": dial tcp 10.42.0.5:8080: connect: connection refused')).toBe("Readiness probe failed: dial tcp: connection refused");
    expect(probeMessage("Readiness probe failed: context deadline exceeded")).toBe("Readiness probe failed: context deadline exceeded");
    expect(probeMessage("Startup probe errored and resulted in unknown state: EOF")).toBe("Startup probe errored and resulted in unknown state: EOF");
  });

  test("a command probe's output never passes, not even behind or in front of a phrase the network stack uses", () => {
    const hidden = "Readiness probe failed: (probe output not shown)";
    expect(probeMessage(`Readiness probe failed: ${PROBE_OUTPUT}`)).toBe(hidden);
    for (const phrase of ["context deadline exceeded", "EOF", "dial tcp 10.42.0.5:8080: connect: connection refused", 'Get "http://10.42.0.5:8080/healthz": EOF', "HTTP probe failed with statuscode: 503"]) {
      expect(probeMessage(`Readiness probe failed: ${phrase} ${PROBE_OUTPUT}`)).toBe(hidden);
      expect(probeMessage(`Readiness probe failed: ${PROBE_OUTPUT} ${phrase}`)).toBe(hidden);
    }
    // A value in the URL's path is not passed through: the reason is shown in its fixed wording.
    const viaPath = probeMessage(`Readiness probe failed: Get "http://10.42.0.5:8080/${PROBE_OUTPUT}": EOF`);
    expect(viaPath).toBe("Readiness probe failed: EOF");
    expect(probeMessage(`Startup probe failed: line one\n${PROBE_OUTPUT}`)).toBe("Startup probe failed: (probe output not shown)");
    // A message that is not in the kubelet's wording is not shown at all.
    expect(probeMessage(`Readiness probe failed <${PROBE_OUTPUT}>: HTTP probe failed with statuscode: 503`)).toBe("(probe message not shown)");
    expect(probeMessage(PROBE_OUTPUT)).toBe("(probe message not shown)");
  });
});

describe("in the report and the failure issue", () => {
  const report = (diagnostics: string): Report => ({
    startedAt: "2026-10-15T01:00:00Z",
    finishedAt: "2026-10-15T01:20:00Z",
    k3sImage: "rancher/k3s:test",
    ok: false,
    shapes: [{ shape: "saas", ok: false, checks: [{ name: "every workload of the release is ready", ok: false, detail: "timed out", durationMs: 1000 }], diagnostics, durationMs: 2000 }],
  });
  const info: RunInfo = { runUrl: "https://example.test/run/1", sha: "0123456789abcdef", eventName: "schedule", result: "failure", failedSteps: [] };

  test("carry what the diagnostics show and none of the planted values", async () => {
    const text = await gatherDiagnostics(NAMESPACE, { kubectl: fakeCluster().kubectl, helmList });
    const value = report(text);
    const markdown = renderMarkdown(value, 1_000_000);
    const issue = failureIssueBody(info, value, markdown);
    for (const output of [markdown, issue, JSON.stringify(value)]) {
      for (const canaryValue of ALL_CANARIES) expect(output).not.toContain(canaryValue);
      expect(output).toContain("last exit 137, OOMKilled");
    }
  });

  test("a line of backticks in an event message cannot end the code fence", () => {
    const markdown = renderMarkdown(report("### Events\nWarning Failed Pod/p: ```\n@someone [link](https://example.test) ```"));
    const fences = markdown.split("\n").filter((line) => line.startsWith("```"));
    expect(fences).toHaveLength(2);
    expect(markdown).toContain("Warning Failed Pod/p: '''");
  });
});

describe("the nightly code", () => {
  const sources = readdirSync(import.meta.dir)
    .filter((name) => name.endsWith(".ts") && !name.endsWith(".test.ts"))
    .map((name) => ({ name, text: readFileSync(join(import.meta.dir, name), "utf8") }));

  test("never asks kubectl for logs or a description: they print what an application or a spec holds", () => {
    // The test files name them (they assert they are never run); the code must not.
    for (const { name, text } of sources) {
      expect({ name, found: /["'](logs|describe)["']/.exec(text)?.[0] }).toEqual({ name, found: undefined });
    }
  });

  test("keeps the output of a JSON read out of the run log, which is an artifact", () => {
    // `kubectl get ... --output json` prints Pod specs, with their environment values. The code
    // that parses it reads it through `kubectlJson` (quiet), never through `kubectl`, whose
    // output `run` writes to the log.
    const smoke = sources.find((source) => source.name === "smoke.ts")?.text ?? "";
    expect(smoke).toContain("kubectlJson");
    const loud = [...smoke.matchAll(/await kubectl\(([\s\S]*?)\);/g)].filter((call) => /"json"|jsonpath/.test(call[1] ?? ""));
    expect(loud.map((call) => call[0])).toEqual([]);
  });
});
