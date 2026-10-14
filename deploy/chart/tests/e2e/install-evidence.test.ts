// Unit tests of the evidence of a failed install (Laysi/trustiot-v2#399). They need no cluster:
// the JSON is what `kubectl get --output json` prints for an identity Deployment whose Pod is
// not ready, and a fake kubectl plays the cluster. Runs with `bun test deploy/chart`.
import { describe, expect, test } from "bun:test";

import { collectInstallEvidence, formatEvents, formatInstallEvidence, KEPT_TAIL_CHARS, MAX_EVENTS, MAX_FIELD_CHARS, oneLine } from "./install-evidence.ts";

// A value the test must never see in the evidence: it stands for an environment value or a
// Secret's data, and is made at run time so that no credential-looking literal is in the file.
const SECRET_VALUE = `${crypto.randomUUID()}${crypto.randomUUID()}`;

const owner = (kind: string, name: string) => [{ kind, name }];

const DEPLOYMENT = {
  items: [
    {
      metadata: { name: "trustiot-identity", generation: 1 },
      spec: { replicas: 1 },
      status: {
        observedGeneration: 1,
        replicas: 1,
        updatedReplicas: 1,
        conditions: [
          { type: "Available", status: "False", reason: "MinimumReplicasUnavailable", message: "Deployment does not have minimum availability." },
          { type: "Progressing", status: "True", reason: "ReplicaSetUpdated", message: 'ReplicaSet "trustiot-identity-7d9f" is progressing.' },
        ],
      },
    },
  ],
};

const REPLICA_SET = {
  items: [
    {
      metadata: { name: "trustiot-identity-7d9f", ownerReferences: owner("Deployment", "trustiot-identity") },
      spec: { replicas: 1 },
      status: { replicas: 1, readyReplicas: 0 },
    },
  ],
};

const POD = {
  items: [
    {
      metadata: { name: "trustiot-identity-7d9f-abcde", ownerReferences: owner("ReplicaSet", "trustiot-identity-7d9f") },
      // The spec is never read into the evidence.
      spec: {
        containers: [
          {
            name: "identity",
            env: [
              { name: "PLAIN", value: SECRET_VALUE },
              { name: "FROM_SECRET", valueFrom: { secretKeyRef: { name: "trustiot-secret", key: "k" } } },
            ],
          },
        ],
      },
      status: {
        phase: "Running",
        conditions: [
          { type: "Ready", status: "False", reason: "ContainersNotReady", message: "containers with unready status: [identity]" },
        ],
        containerStatuses: [
          {
            name: "identity",
            ready: false,
            restartCount: 2,
            state: { waiting: { reason: "CrashLoopBackOff", message: "back-off 20s restarting failed container" } },
            lastState: { terminated: { exitCode: 1, reason: "Error", message: SECRET_VALUE } },
          },
        ],
      },
    },
    {
      metadata: { name: "trustiot-assets-1" },
      status: { phase: "Running", conditions: [{ type: "Ready", status: "True" }], containerStatuses: [{ name: "assets", ready: true, restartCount: 0, state: { running: {} } }] },
    },
  ],
};

const EVENTS = {
  items: [
    { type: "Warning", reason: "Unhealthy", involvedObject: { kind: "Pod", name: "trustiot-identity-7d9f-abcde" }, message: "Readiness probe failed: HTTP probe failed with statuscode: 503", lastTimestamp: "2026-10-14T07:12:30Z", count: 3 },
    { type: "Normal", reason: "Scheduled", involvedObject: { kind: "Pod", name: "trustiot-identity-7d9f-abcde" }, message: "Successfully assigned nightly-e2e-saas/trustiot-identity-7d9f-abcde to k3d-nightly-agent-0", lastTimestamp: "2026-10-14T07:12:26Z" },
    { type: "Normal", reason: "ScalingReplicaSet", involvedObject: { kind: "Deployment", name: "trustiot-identity" }, message: "Scaled up replica set trustiot-identity-7d9f to 1", eventTime: "2026-10-14T07:12:25Z" },
  ],
};

describe("formatInstallEvidence", () => {
  const evidence = formatInstallEvidence({
    events: EVENTS,
    deployments: DEPLOYMENT,
    replicaSets: REPLICA_SET,
    pods: POD,
    history: "REVISION\tSTATUS\tDESCRIPTION\n1\tfailed\tRelease \"trustiot\" failed: resource not ready\n",
  });

  test("shows the Deployment's counts and conditions", () => {
    expect(evidence).toContain("Deployment/trustiot-identity: desired=1 replicas=1 updated=1 ready=0 available=0 generation=1/observed=1");
    expect(evidence).toContain("Available=False (MinimumReplicasUnavailable: Deployment does not have minimum availability.)");
    expect(evidence).toContain("Progressing=True (ReplicaSetUpdated:");
  });

  test("shows the ReplicaSet with its owner", () => {
    expect(evidence).toContain("ReplicaSet/trustiot-identity-7d9f: owner=Deployment/trustiot-identity desired=1 replicas=1 ready=0 available=0");
  });

  test("shows why a Pod is not ready: phase, condition, container state and restarts", () => {
    expect(evidence).toContain("Pod/trustiot-identity-7d9f-abcde: owner=ReplicaSet/trustiot-identity-7d9f phase=Running");
    expect(evidence).toContain("Ready=False (ContainersNotReady: containers with unready status: [identity])");
    expect(evidence).toContain("identity: waiting (CrashLoopBackOff: back-off 20s restarting failed container), ready=false, restarts=2; last exit 1, Error");
    expect(evidence).toContain("assets: running, ready=true, restarts=0");
  });

  test("lists events oldest first with their object, count and message", () => {
    const lines = evidence.split("\n").filter((line) => line.includes("Scale") || line.includes("Scheduled") || line.includes("Unhealthy"));
    expect(lines.map((line) => line.split(" ")[0])).toEqual(["2026-10-14T07:12:25Z", "2026-10-14T07:12:26Z", "2026-10-14T07:12:30Z"]);
    expect(evidence).toContain("Warning Unhealthy Pod/trustiot-identity-7d9f-abcde x3: Readiness probe failed: HTTP probe failed with statuscode: 503");
  });

  test("adds the release history", () => {
    expect(evidence).toContain("### helm history");
    expect(evidence).toContain("failed");
  });

  test("never prints an environment value, a termination message or the Pod spec", () => {
    expect(evidence).not.toContain(SECRET_VALUE);
    expect(evidence).not.toContain("secretKeyRef");
    expect(evidence).not.toContain("trustiot-secret");
    expect(evidence).not.toContain("PLAIN");
  });

  test("reports a section that could not be read and keeps the others", () => {
    const partial = formatInstallEvidence({ events: new Error("the server was unreachable"), deployments: DEPLOYMENT, replicaSets: { items: [] }, pods: { items: [] } });
    expect(partial).toContain("### Events\n(not available: the server was unreachable)");
    expect(partial).toContain("### ReplicaSets\n(none)");
    expect(partial).toContain("Deployment/trustiot-identity");
  });
});

describe("free text", () => {
  test("is one line, cut at the limit", () => {
    expect(oneLine("a\n  b\tc")).toBe("a b c");
    const long = "x".repeat(MAX_FIELD_CHARS + 50);
    expect(oneLine(long)).toBe(`${"x".repeat(MAX_FIELD_CHARS - KEPT_TAIL_CHARS)}... (50 characters cut) ...${"x".repeat(KEPT_TAIL_CHARS)}`);
  });

  test("a long message keeps its start and its end, where a chain of errors names the cause", () => {
    const message = `Failed to pull image: ${"a ".repeat(200)}dial tcp: lookup registry.example.test: no such host`;
    const line = oneLine(message);
    expect(line.startsWith("Failed to pull image: a a")).toBe(true);
    expect(line.endsWith("lookup registry.example.test: no such host")).toBe(true);
    expect(line).toContain(" characters cut) ...");
  });

  test("a URL's user info and query string are removed (Laysi/trustiot-v2#410)", () => {
    const token = `${crypto.randomUUID()}${crypto.randomUUID()}`;
    const line = oneLine(`pull failed: https://user:${token}@registry.example.test/v2/x/manifests/y?token=${token}&scope=z 401`);
    expect(line).toBe("pull failed: https://(user info removed)@registry.example.test/v2/x/manifests/y?(query removed) 401");
    expect(oneLine('Get "http://10.42.0.5:8080/healthz": connection refused')).toBe('Get "http://10.42.0.5:8080/healthz": connection refused');
  });

  test("a failed probe's output is shown only when the kubelet or the network wrote it", () => {
    const output = `${crypto.randomUUID()}${crypto.randomUUID()}`;
    const event = (message: string) => ({ type: "Warning", reason: "Unhealthy", involvedObject: { kind: "Pod", name: "p" }, message, lastTimestamp: "2026-10-15T00:00:00Z" });
    const lines = formatEvents({ items: [event(`Readiness probe failed: ${output}`), event("Readiness probe failed: HTTP probe failed with statuscode: 503")] });
    expect(lines.join("\n")).not.toContain(output);
    expect(lines[0]).toContain("Readiness probe failed: (probe output not shown)");
    expect(lines[1]).toContain("Readiness probe failed: HTTP probe failed with statuscode: 503");
  });

  test("a multi-line message cannot inject a line into the evidence", () => {
    const lines = formatEvents({ items: [{ type: "Warning", reason: "Failed", involvedObject: { kind: "Pod", name: "p" }, message: "first\n### Deployments\nsecond", lastTimestamp: "2026-10-14T00:00:00Z" }] });
    expect(lines).toHaveLength(1);
    expect(lines[0]).toContain("first ### Deployments second");
  });

  test("only the newest events are kept, and the cut is said", () => {
    const items = Array.from({ length: MAX_EVENTS + 5 }, (_, index) => ({
      reason: `R${index}`,
      involvedObject: { kind: "Pod", name: "p" },
      lastTimestamp: `2026-10-14T00:${String(Math.floor(index / 60)).padStart(2, "0")}:${String(index % 60).padStart(2, "0")}Z`,
    }));
    const lines = formatEvents({ items });
    expect(lines).toHaveLength(MAX_EVENTS + 1);
    expect(lines[0]).toBe("(5 older events not shown)");
    expect(lines.at(-1)).toContain(`R${MAX_EVENTS + 4}`);
    expect(lines.join("\n")).not.toContain(" R4 ");
  });

  test("tolerates objects without status", () => {
    const evidence = formatInstallEvidence({ events: {}, deployments: { items: [{ metadata: { name: "d" } }] }, replicaSets: null, pods: { items: [{}] } });
    expect(evidence).toContain("Deployment/d: desired=1 replicas=0");
    expect(evidence).toContain("Pod/(no name): phase=unknown");
  });
});

describe("collectInstallEvidence", () => {
  const json: Record<string, unknown> = { pods: POD, events: EVENTS, deployments: DEPLOYMENT, replicasets: REPLICA_SET };

  test("runs only \`kubectl get\` of events, Deployments, ReplicaSets and Pods: no logs, Secrets or ConfigMaps", () => {
    const calls: string[][] = [];
    const evidence = collectInstallEvidence(
      (args) => {
        calls.push(args);
        return JSON.stringify(json[args[1] ?? ""]);
      },
      () => "REVISION\tSTATUS\n1\tfailed\n",
    );
    expect(calls.every((args) => args[0] === "get" && args.slice(2).join(" ") === "--output json")).toBe(true);
    expect(calls.map((args) => args[1]).sort()).toEqual(["deployments", "events", "pods", "replicasets"]);
    expect(evidence).toContain("Deployment/trustiot-identity");
    expect(evidence).toContain("### helm history");
    expect(evidence).not.toContain(SECRET_VALUE);
  });

  test("a kubectl that fails does not stop the collection", () => {
    const evidence = collectInstallEvidence(
      (args) => {
        if (args[1] === "events") throw new Error("kubectl get failed (1): the connection to the server was refused");
        return JSON.stringify(json[args[1] ?? ""]);
      },
      () => {
        throw new Error("helm history failed");
      },
    );
    expect(evidence).toContain("### Events\n(not available: kubectl get failed (1): the connection to the server was refused)");
    expect(evidence).toContain("### helm history\n(not available: helm history failed)");
    expect(evidence).toContain("Deployment/trustiot-identity");
  });

  test("output that is not JSON is reported in its section", () => {
    const evidence = collectInstallEvidence(() => "not json", () => "");
    expect(evidence).toContain("### Pods\n(not available:");
  });
});
