// The demo isolation probe of scripts/demo/dashboard-check.ts against a fake demo cluster, without
// ssh (Laysi/trustiot-v2#881): the plan and its refusals, what it prints, the order of the probes
// and the restarts, and the ssh command the probe containers run through.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  DNS_QUERY_ID,
  type IsolationTargets,
  type Probe,
  type ProbeResult,
  PROBE_CONTAINER_LIMIT_MS,
  isolationPlan,
  probeCommand,
} from "../nightly/dashboard-check.ts";
import type { ExecOptions, ExecResult } from "../nightly/exec.ts";
import type { K8sObject } from "../nightly/manifests.ts";
import { Box, type CommonOptions, Log } from "./lib.ts";
import {
  type Facts,
  KUBECTL,
  type ProbeOps,
  REMOTE_COMMAND_LIMIT,
  ROLES,
  type Role,
  type RolloutState,
  SSH_OPTIONS,
  type Setup,
  ShownLog,
  boxOps,
  changeOf,
  containerName,
  deploymentsOf,
  expectationText,
  planProbe,
  podFactsOf,
  remoteCommand,
  resultLines,
  rolledOut,
  rolloutStateOf,
  runProbe,
  shownText,
  sshKubectlExec,
} from "./dashboard-check.ts";

/** A node address outside the cluster's networks (TEST-NET-1), as the API server's endpoint on the demo machine. */
const NODE_ADDRESS = "192.0.2.10";
const NODE = "demo-node-x";
const SSH_HOST = "demo-ssh";
const IMAGE = `ghcr.io/laysi/trustiot-v2/postgres:0.0.0-rc.16@sha256:${"ab".repeat(32)}`;
const PODS = { checker: "trustiot-dashboard-check-7d9-aaaaa", control: "trustiot-identity-65f-bbbbb", assistant: "trustiot-assistant-78c-ccccc" };
const DEPLOYMENTS: Record<Role, string> = { checker: "trustiot-dashboard-check", control: "trustiot-identity", assistant: "trustiot-assistant" };
const TARGETS: IsolationTargets = {
  namespace: "trustiot",
  checker: { service: "trustiot-dashboard-check", clusterIp: "10.43.20.5", port: 8080, podIp: "10.42.0.31", healthPort: 8081 },
  collector: { clusterIp: "10.43.20.9", port: 4318 },
  apiServer: { clusterIp: "10.43.0.1", port: 443, endpoints: [{ host: NODE_ADDRESS, port: 6443 }] },
  clusterDns: { clusterIp: "10.43.0.10", udpPort: 53, tcpPort: 53 },
  checkBody: '{"page":"<p>x</p>"}',
};
const ROLLED: RolloutState = { generation: 3, observedGeneration: 3, desired: 1, updated: 1, total: 1, available: 1 };

const ROLES_FACTS = Object.fromEntries(ROLES.map((role) => [role, { deployment: DEPLOYMENTS[role], rollout: ROLLED, node: NODE }])) as Setup["roles"];

/** The demo cluster with the dashboard check deployed; `pods`, `targets` and `roles` go into the setup. */
function factsOf(overrides: Partial<Omit<Facts, "setup">> & Partial<Setup> & { setup?: Facts["setup"] } = {}): Facts {
  const { pods = { ...PODS }, targets = TARGETS, roles = ROLES_FACTS, ...rest } = overrides;
  return {
    release: { chartVersion: "0.0.0-rc.16+0123456789ab", status: "deployed" },
    podPids: { lines: ["node <demo-node>: podPidsLimit 16384"] },
    setup: { pods, targets, roles },
    postgres: [{ image: IMAGE, node: NODE }],
    nodes: [NODE],
    ...rest,
  };
}

// --- Answers of a cluster that keeps the chart's promises (as in scripts/nightly/dashboard-check.test.ts) ---

const answered = (text: string): ProbeResult => ({ kind: "answered", bytes: text });
const http = (status: number, body = "") => `HTTP/1.1 ${status} X\r\ncontent-length: ${body.length}\r\n\r\n${body}`;
const TLS_ALERT = "\u0015\u0003\u0003\u0000\u0002\u00022";
function dnsResponse({ flags = 0x8180, answers = 1 } = {}): string {
  const header = [DNS_QUERY_ID >> 8, DNS_QUERY_ID & 0xff, flags >> 8, flags & 0xff, 0, 1, 0, answers, 0, 0, 0, 0];
  const question = [7, ...Buffer.from("example"), 3, ...Buffer.from("com"), 0, 0, 1, 0, 1];
  const record = [0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 93, 184, 215, 14];
  return String.fromCharCode(...header, ...question, ...Array.from({ length: answers }, () => record).flat());
}
const SERVFAIL = dnsResponse({ flags: 0x8182, answers: 0 });
const tcpFramed = (message: string) => String.fromCharCode(message.length >> 8, message.length & 0xff) + message;

function isolated(pod: string, probe: Probe): ProbeResult {
  if (pod === PODS.checker) {
    if (probe.id.startsWith("ztunnel-hbone-")) return answered(TLS_ALERT);
    if (probe.kind === "bytes" && probe.host === "127.0.0.1") return answered(probe.transport === "tcp" ? tcpFramed(SERVFAIL) : SERVFAIL);
    return probe.id === "loopback" ? answered(http(200)) : probe.kind === "dns" ? { kind: "unresolved" } : { kind: "silent", exit: 0 };
  }
  if (pod === PODS.control) {
    if (probe.kind === "dns") return { kind: "resolved", address: "10.43.0.1" };
    if (probe.kind === "bytes") return probe.transport === "udp" ? answered(dnsResponse()) : answered("\u00000");
    return probe.id.startsWith("checker") ? { kind: "silent", exit: 124 } : answered(http(404));
  }
  return answered(http(200, '{"verdict":"passed"}'));
}

// --- What may be printed ---------------------------------------------------------------------------

describe("shownText", () => {
  test("masks the node, the ssh host and addresses outside the cluster, and keeps cluster addresses and the probes' public targets", () => {
    const text = `node ${NODE}: podPidsLimit 16384; ssh: connect to host ${SSH_HOST}: refused; the API server at ${NODE_ADDRESS}:6443; 1.1.1.1:80 outside the cluster; DNS example.com. over UDP; DNS kubernetes.default.svc.cluster.local.: resolved to 10.43.0.1; POST on trustiot-dashboard-check.trustiot.svc.cluster.local:8080; pod 10.42.0.31; image ${IMAGE}; lan.example.com 198.51.100.7`;
    const shown = shownText(text, { sshHost: SSH_HOST, nodes: [NODE] });
    expect(shown).toContain("node <demo-node>: podPidsLimit 16384");
    expect(shown).toContain("connect to host <ssh-host>");
    expect(shown).toContain("the API server at <address>:6443");
    expect(shown).toContain("1.1.1.1:80 outside the cluster");
    expect(shown).toContain("DNS example.com. over UDP");
    expect(shown).toContain("kubernetes.default.svc.cluster.local.: resolved to 10.43.0.1");
    expect(shown).toContain("trustiot-dashboard-check.trustiot.svc.cluster.local:8080");
    expect(shown).toContain("pod 10.42.0.31");
    expect(shown).toContain(`image ${IMAGE}`);
    expect(shown).toContain("<host> <address>");
    for (const hidden of [NODE, SSH_HOST, NODE_ADDRESS, "198.51.100.7", "lan.example.com"]) expect(shown).not.toContain(hidden);
  });

  test("keeps only the exact outside address and name, not longer ones that contain them", () => {
    const shown = shownText("11.1.1.1 1.1.1.10 1.1.1.1.5 sub.example.com", {});
    for (const hidden of ["11.1.1.1", "1.1.1.10", "1.1.1.5", "sub.example.com"]) expect(shown).not.toContain(hidden);
  });
});

// --- Rollouts ---------------------------------------------------------------------------------------

describe("rollouts", () => {
  test("reads a Deployment's rollout as kubectl rollout status does", () => {
    const deployment = { metadata: { generation: 4 }, spec: { replicas: 1 }, status: { observedGeneration: 4, updatedReplicas: 1, replicas: 1, availableReplicas: 1 } };
    expect(rolloutStateOf(deployment)).toEqual({ generation: 4, observedGeneration: 4, desired: 1, updated: 1, total: 1, available: 1 });
    expect(rolledOut(rolloutStateOf(deployment))).toBe(true);
    // Not yet observed, an old pod still there, the new one not available, nothing at all.
    expect(rolledOut({ ...ROLLED, observedGeneration: 2 })).toBe(false);
    expect(rolledOut({ ...ROLLED, total: 2 })).toBe(false);
    expect(rolledOut({ ...ROLLED, available: 0 })).toBe(false);
    expect(rolledOut(rolloutStateOf({}))).toBe(false);
  });
});

// --- Facts and plan --------------------------------------------------------------------------------

describe("podFactsOf and deploymentsOf", () => {
  test("finds each probed pod's node, the running Postgres instance pods' image, and one Deployment per role", () => {
    const pod = (name: string, labels: Record<string, string>, containers: { name: string; image: string }[], phase = "Running", deleting = false) => ({
      metadata: { name, labels, ...(deleting ? { deletionTimestamp: "t" } : {}) },
      spec: { nodeName: NODE, containers },
      status: { phase },
    });
    const list = {
      items: [
        pod(PODS.checker, {}, []),
        pod(PODS.control, {}, []),
        pod(PODS.assistant, {}, []),
        pod("trustiot-postgres-1", { "cnpg.io/cluster": "trustiot-postgres" }, [{ name: "postgres", image: IMAGE }, { name: "other", image: "x" }]),
        pod("trustiot-postgres-2", { "cnpg.io/cluster": "trustiot-postgres" }, [{ name: "postgres", image: "old" }], "Running", true),
        pod("trustiot-postgres-3", { "cnpg.io/cluster": "trustiot-postgres" }, [{ name: "postgres", image: "pending" }], "Pending"),
      ],
    };
    expect(podFactsOf(list, PODS)).toEqual({ nodes: { checker: NODE, control: NODE, assistant: NODE }, postgres: [{ image: IMAGE, node: NODE }] });
    const deployment = (name: string, component: string): K8sObject => ({ kind: "Deployment", metadata: { name, labels: { "app.kubernetes.io/component": component } } });
    const found = deploymentsOf([deployment("a", "dashboard-check"), deployment("b", "identity"), deployment("c", "assistant"), deployment("d", "assistant"), { kind: "Service", metadata: { name: "e", labels: { "app.kubernetes.io/component": "identity" } } }]);
    expect(found.checker?.metadata?.name).toBe("a");
    expect(found.control?.metadata?.name).toBe("b");
    // Two Deployments of one component: none is taken.
    expect(found.assistant).toBeUndefined();
  });
});

describe("planProbe", () => {
  test("plans the nightly's probes from the three pods with the Postgres image, and restarts their Deployments", () => {
    const plan = planProbe(factsOf());
    expect(plan.blockers).toEqual([]);
    expect(plan.image).toBe(IMAGE);
    expect(plan.probes).toEqual(isolationPlan(TARGETS));
    expect(plan.restarts).toEqual(DEPLOYMENTS);
    expect(plan.ok).toContain("HelmRelease trustiot/trustiot: latest 0.0.0-rc.16+0123456789ab (deployed)");
    expect(plan.ok).toContain("kubelet: node <demo-node>: podPidsLimit 16384");
    expect(plan.ok).toContain(`probe image (the release's Postgres image, already on the node): ${IMAGE}`);
    expect(plan.ok).toContain(`the assistant's pod: ${PODS.assistant}`);
  });

  test.each([
    ["the release is not deployed", { release: { chartVersion: "0.0.0-rc.16+0123456789ab", status: "failed" } }, "(failed): wait until Flux has deployed the release"],
    ["the release has no history", { release: undefined }, "has no history yet"],
    ["the kubelet has no process limit", { podPids: { lines: [], problem: "node <demo-node>: podPidsLimit -1, expected 16384" } }, "podPidsLimit -1, expected 16384"],
    ["no Postgres pod runs", { postgres: [] }, "no Postgres instance pod runs"],
    ["the Postgres pods run two images", { postgres: [{ image: IMAGE, node: NODE }, { image: `${IMAGE.slice(0, -1)}c`, node: NODE }] }, "run 2 images"],
    ["the Postgres image is not ours or not pinned", { postgres: [{ image: "ghcr.io/example/postgres:18", node: NODE }] }, "not deploy/images/postgres pinned by digest"],
    ["a probed pod runs on another node", { postgres: [{ image: IMAGE, node: "other-node" }] }, "runs on another node than the Postgres pod"],
    ["a Deployment is rolling out", { roles: { ...ROLES_FACTS, assistant: { deployment: DEPLOYMENTS.assistant, rollout: { ...ROLLED, total: 2 }, node: NODE } } }, "deployment/trustiot-assistant is still rolling out"],
    ["a role has no single Deployment", { roles: { ...ROLES_FACTS, control: { deployment: undefined, rollout: undefined, node: NODE } } }, "no single Deployment of identity"],
    ["the dashboard check is not deployed yet", { setup: { problem: "the release should have one Service of dashboard-check, found 0" } }, "the dashboard check cannot be probed: the release should have one Service of dashboard-check, found 0"],
  ] as [string, Parameters<typeof factsOf>[0], string][])("refuses when %s", (_label, overrides, message) => {
    expect(planProbe(factsOf(overrides)).blockers.join("\n")).toContain(message);
  });

  test("refuses a probe container whose command line would not reach the demo machine", () => {
    const plan = planProbe(factsOf({ targets: { ...TARGETS, checkBody: JSON.stringify({ page: "x".repeat(REMOTE_COMMAND_LIMIT) }) } }));
    expect(plan.blockers.join("\n")).toContain(`more than ${REMOTE_COMMAND_LIMIT}`);
    // Counted in bytes, as the kernel counts the argument: 中 is one character but three bytes.
    const wide = planProbe(factsOf({ targets: { ...TARGETS, checkBody: JSON.stringify({ page: "中".repeat(Math.ceil(REMOTE_COMMAND_LIMIT / 3)) }) } }));
    expect(wide.blockers.join("\n")).toContain(`more than ${REMOTE_COMMAND_LIMIT}`);
  });

  test("the confirmation covers the pods, the image, the probes and the restarts, not the advice", () => {
    const code = (facts: Facts) => JSON.stringify(changeOf(planProbe(facts)));
    expect(code(factsOf())).toBe(code(factsOf({ podPids: { lines: ["another line"] } })));
    expect(code(factsOf({ pods: { ...PODS, checker: "trustiot-dashboard-check-7d9-zzzzz" } }))).not.toBe(code(factsOf()));
    expect(code(factsOf({ targets: { ...TARGETS, checkBody: '{"page":"<p>y</p>"}' } }))).not.toBe(code(factsOf()));
    expect(code(factsOf({ postgres: [{ image: `${IMAGE.slice(0, -1)}c`, node: NODE }] }))).not.toBe(code(factsOf()));
  });

  test("says what each probe expects", () => {
    expect(expectationText({ open: false })).toBe("no answer");
    expect(expectationText({ open: false, tlsRefusal: true })).toBe("no answer, or only a TLS alert");
    expect(expectationText({ open: false, dnsRefusal: true })).toBe("no answer, or a DNS error without a record");
    expect(expectationText({ open: true, resolved: true })).toBe("the name resolves");
    expect(expectationText({ open: true, dnsAnswer: true })).toBe("a DNS answer with a record");
    expect(expectationText({ open: true })).toBe("an answer");
    expect(expectationText({ open: true, status: 200, verdict: "passed" })).toBe("HTTP 200 with the verdict passed");
  });
});

// --- The ssh command ----------------------------------------------------------------------------------

describe("the probe containers over ssh", () => {
  test("the remote command line is kubectl's arguments as the demo machine's shell reads them back", () => {
    // What a probe container gets: the nightly's script (quotes, $, newlines) and a request with
    // quotes and non-ASCII text.
    const plan = isolationPlan({ ...TARGETS, checkBody: `{"page":"<p>it's \\"中\\" $HOME \`id\`</p>"}` });
    for (const role of ROLES) {
      const own = plan.filter((entry) => entry.from === role).map((entry) => entry.probe);
      const cmd = probeCommand(KUBECTL, "trustiot", PODS[role], containerName(role, "abc123"), own, IMAGE);
      const remote = remoteCommand(cmd);
      expect(remote.startsWith("sudo k3s kubectl debug --namespace trustiot ")).toBe(true);
      // The demo machine's login shell is bash: print each argument it reads, NUL-separated.
      const shell = Bun.spawnSync(["bash", "-c", `args() { shift 3; printf '%s\\0' "$@"; }; args ${remote}`]);
      expect(shell.exitCode).toBe(0);
      const back = new TextDecoder().decode(shell.stdout).split("\0").slice(0, -1);
      expect(back).toEqual(cmd.slice(1));
    }
    expect(() => remoteCommand(["/bin/sh", "-c", "id"])).toThrow("only kubectl runs on the demo machine");
  });

  test("runs ssh without a prompt and with keepalives, keeps kubectl's input open, passes no KUBECONFIG and masks the host", async () => {
    const calls: { cmd: string[]; options: ExecOptions }[] = [];
    const run = async (cmd: string[], options: ExecOptions): Promise<ExecResult> => {
      calls.push({ cmd, options });
      return { code: 255, stdout: "", stderr: `ssh: Could not resolve hostname ${SSH_HOST}: Name or service not known\n`, timedOut: false };
    };
    const result = await sshKubectlExec(SSH_HOST, run)([KUBECTL, "get", "pod", "a b"], { env: { KUBECONFIG: "/x" }, stdin: "go\n", keepStdinOpen: true, timeoutMs: PROBE_CONTAINER_LIMIT_MS });
    expect(calls).toEqual([{ cmd: ["ssh", ...SSH_OPTIONS, SSH_HOST, "sudo k3s kubectl get pod 'a b'"], options: { stdin: "go\n", keepStdinOpen: true, timeoutMs: PROBE_CONTAINER_LIMIT_MS } }]);
    expect(SSH_OPTIONS).toContain("BatchMode=yes");
    expect(result.stderr).toBe("ssh: Could not resolve hostname <ssh-host>: Name or service not known\n");
    await expect(sshKubectlExec(SSH_HOST, run)([KUBECTL, "x".repeat(REMOTE_COMMAND_LIMIT)], {})).rejects.toThrow(`more than ${REMOTE_COMMAND_LIMIT}`);
    await expect(sshKubectlExec(SSH_HOST, run)([KUBECTL, "中".repeat(Math.ceil(REMOTE_COMMAND_LIMIT / 3))], {})).rejects.toThrow(`more than ${REMOTE_COMMAND_LIMIT}`);
  });
});

// --- The run --------------------------------------------------------------------------------------

interface Cluster {
  facts: Facts;
  events: string[];
  lockHeld: boolean;
  /** Pods that still exist, by name. */
  pods: Set<string>;
  /** A probe result to put in place of the isolated one. */
  change?: (pod: string, probe: Probe) => ProbeResult | undefined;
  /** Polls until a restarted Deployment has rolled out. */
  rolloutPolls: number;
  now: number;
  /** Facts the second read returns (under the lock). */
  factsUnderLock?: Facts;
}

function fakeOps(cluster: Cluster, overrides: Partial<ProbeOps> = {}): ProbeOps {
  let reads = 0;
  const restarted = new Map<string, number>();
  return {
    tunnel: async () => ({ active: "active", pid: "7", restarts: "0", started: "t" }),
    async facts() {
      reads += 1;
      return reads > 1 && cluster.factsUnderLock !== undefined ? cluster.factsUnderLock : cluster.facts;
    },
    async lock() {
      cluster.events.push("lock");
      cluster.lockHeld = true;
      return async () => {
        cluster.events.push("unlock");
        cluster.lockHeld = false;
      };
    },
    async probe(pod, name, image, probes) {
      expect(cluster.lockHeld).toBe(true);
      cluster.events.push(`probe ${pod} ${name} ${image === IMAGE ? "image" : image} ${probes.length}`);
      return new Map(probes.map((probe) => [probe.id, cluster.change?.(pod, probe) ?? isolated(pod, probe)]));
    },
    async restart(deployment) {
      expect(cluster.lockHeld).toBe(true);
      cluster.events.push(`restart ${deployment}`);
      restarted.set(deployment, 0);
    },
    async rollout(deployment) {
      const polls = (restarted.get(deployment) ?? 0) + 1;
      restarted.set(deployment, polls);
      if (polls <= cluster.rolloutPolls) return { ...ROLLED, generation: 4, total: 2 };
      // The old pod is gone once the Deployment has rolled out.
      const role = ROLES.find((r) => DEPLOYMENTS[r] === deployment)!;
      cluster.pods.delete(PODS[role]);
      return { ...ROLLED, generation: 4, observedGeneration: 4 };
    },
    async podExists(pod) {
      return cluster.pods.has(pod);
    },
    now: () => cluster.now,
    async sleep(ms) {
      cluster.now += ms;
    },
    ...overrides,
  };
}

let dir: string;
beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "dashboard-check-"));
  chmodSync(dir, 0o700);
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

const options = (apply: boolean, confirm = ""): CommonOptions => ({ sshHost: SSH_HOST, apply, confirm, lockHeld: false });
const clusterOf = (overrides: Partial<Cluster> = {}): Cluster => ({ facts: factsOf(), events: [], lockHeld: false, pods: new Set(Object.values(PODS)), rolloutPolls: 2, now: 0, ...overrides });
const quiet = <T>(body: () => Promise<T>): Promise<T> => {
  const log = console.log;
  console.log = () => {};
  return body().finally(() => {
    console.log = log;
  });
};
const quietSync = (body: () => void): void => {
  const log = console.log;
  console.log = () => {};
  try {
    body();
  } finally {
    console.log = log;
  }
};
/** Runs a dry run and returns the confirmation code it printed. */
async function dryRun(cluster: Cluster): Promise<{ code: string; text: string }> {
  const log = new Log("dashboard-check", dir);
  await quiet(() => runProbe(fakeOps(cluster), log, options(false), { sshHost: SSH_HOST }, "s1"));
  const text = readFileSync(log.file, "utf8");
  const code = /--apply --confirm ([0-9a-f]{12})/.exec(text)?.[1];
  if (code === undefined) throw new Error(`no confirmation code in:\n${text}`);
  return { code, text };
}

describe("runProbe", () => {
  test("a dry run only reads and prints every probe, the restarts and a code, without the node's name or address", async () => {
    const cluster = clusterOf();
    const { text } = await dryRun(cluster);
    expect(cluster.events).toEqual([]);
    expect(text).toContain("probe: from the dashboard check's pod, DNS kubernetes.default.svc.cluster.local.: expects no answer");
    expect(text).toContain("probe: from the dashboard check's pod, the API server at <address>:6443: expects no answer");
    expect(text).toContain("probe: from the identity service's pod, 1.1.1.1:80 outside the cluster: expects an answer");
    expect(text).toContain("probe: from the assistant's pod, POST /internal/dashboard-check/v1/check on trustiot-dashboard-check.trustiot.svc.cluster.local:8080 with the sandbox's correct fixture page: expects HTTP 200 with the verdict passed");
    expect(text).toContain("then: rollout restart deployment/trustiot-dashboard-check, deployment/trustiot-identity, deployment/trustiot-assistant");
    expect(text).toContain("dry run: nothing changed. To make these changes, run again with: bun scripts/demo/dashboard-check.ts --ssh-host <host> --apply --confirm ");
    for (const hidden of [NODE, SSH_HOST, NODE_ADDRESS]) expect(text).not.toContain(hidden);
  });

  test("--apply with the code takes the lock, probes from the three pods in order, restarts each Deployment until its probed pod is gone, and passes", async () => {
    const cluster = clusterOf();
    const { code } = await dryRun(cluster);
    const log = new Log("dashboard-check", dir);
    await quiet(() => runProbe(fakeOps(cluster), log, options(true, code), { sshHost: SSH_HOST }, "s1"));
    const own = (role: Role) => isolationPlan(TARGETS).filter((entry) => entry.from === role).length;
    expect(cluster.events).toEqual([
      "lock",
      `probe ${PODS.checker} tq-probe-881-checker-s1 image ${own("checker")}`,
      `probe ${PODS.control} tq-probe-881-control-s1 image ${own("control")}`,
      `probe ${PODS.assistant} tq-probe-881-assistant-s1 image ${own("assistant")}`,
      "restart trustiot-dashboard-check",
      "restart trustiot-identity",
      "restart trustiot-assistant",
      "unlock",
    ]);
    expect(cluster.pods.size).toBe(0);
    const text = readFileSync(log.file, "utf8");
    const total = isolationPlan(TARGETS).length;
    expect(text.match(/ PASS from /g)?.length).toBe(total);
    expect(text).not.toContain(" FAIL ");
    expect(text).toContain("PASS from the dashboard check's pod, the API server at <address>:6443: no answer (closed without a byte) (expects no answer)");
    expect(text).toContain("PASS from the assistant's pod, POST /internal/dashboard-check/v1/check on trustiot-dashboard-check.trustiot.svc.cluster.local:8080 with the sandbox's correct fixture page: answered HTTP 200 (expects HTTP 200 with the verdict passed)");
    expect(text).toContain(`deployment/trustiot-identity rolled out (generation 4/4, 1 updated, 1 available, 1 in all, 1 desired); the probed pod ${PODS.control} is gone`);
    expect(text).toContain(`PASS all ${total} probes`);
    for (const hidden of [NODE, SSH_HOST, NODE_ADDRESS]) expect(text).not.toContain(hidden);
  });

  test("a failed probe still restarts every probed Deployment, prints FAIL and ends with an error", async () => {
    // Another service reaches the dashboard check (the AuthorizationPolicy or NetworkPolicy is gone).
    const cluster = clusterOf({ change: (pod, probe) => (pod === PODS.control && probe.id === "checker-service" ? answered(http(400)) : undefined) });
    const { code } = await dryRun(cluster);
    const log = new Log("dashboard-check", dir);
    await expect(quiet(() => runProbe(fakeOps(cluster), log, options(true, code), { sshHost: SSH_HOST }, "s1"))).rejects.toThrow("the isolation check failed (1 of");
    expect(cluster.events.filter((event) => event.startsWith("restart"))).toEqual(["restart trustiot-dashboard-check", "restart trustiot-identity", "restart trustiot-assistant"]);
    expect(cluster.events.at(-1)).toBe("unlock");
    const text = readFileSync(log.file, "utf8");
    expect(text).toContain("FAIL from the identity service's pod, the dashboard check's Service 10.43.20.5:8080: answered HTTP 400 (expects no answer)");
    expect(text).not.toContain("PASS all");
  });

  test("a probe container that cannot run stops the probes, and only the pods that got one are restarted", async () => {
    const cluster = clusterOf();
    const { code } = await dryRun(cluster);
    const ops = fakeOps(cluster, {
      async probe(pod) {
        cluster.events.push(`probe ${pod}`);
        throw new Error(`probe container in pod ${pod}: kubectl debug exited with 1; ErrImageNeverPull on ${NODE}`);
      },
    });
    const error = await quiet(() => runProbe(ops, new Log("dashboard-check", dir), options(true, code), { sshHost: SSH_HOST }, "s1")).catch((e: unknown) => e);
    expect(String(error)).toContain("ErrImageNeverPull on <demo-node>");
    expect(String(error)).not.toContain(NODE);
    expect(cluster.events).toEqual(["lock", `probe ${PODS.checker}`, "restart trustiot-dashboard-check", "unlock"]);
  });

  test("a Deployment that does not roll out in time is reported with the command to restart it, after the others", async () => {
    const cluster = clusterOf({ rolloutPolls: Number.MAX_SAFE_INTEGER });
    const { code } = await dryRun(cluster);
    const log = new Log("dashboard-check", dir);
    await expect(quiet(() => runProbe(fakeOps(cluster), log, options(true, code), { sshHost: SSH_HOST }, "s1"))).rejects.toThrow("restarting the probed Deployments failed: deployment/trustiot-dashboard-check: timed out after 600 s");
    expect(cluster.events.filter((event) => event.startsWith("restart"))).toHaveLength(3);
    expect(readFileSync(log.file, "utf8")).toContain("restart it by hand: sudo k3s kubectl -n trustiot rollout restart deployment/trustiot-assistant");
  });

  test("--apply refuses a wrong code, blockers, and a plan that changed under the lock, without probing", async () => {
    const cluster = clusterOf();
    const { code } = await dryRun(cluster);
    await expect(quiet(() => runProbe(fakeOps(cluster), new Log("dashboard-check", dir), options(true, "000000000000"), { sshHost: SSH_HOST }, "s1"))).rejects.toThrow("the plan changed since it was shown");
    const blocked = clusterOf({ facts: factsOf({ release: { chartVersion: "v", status: "pending-upgrade" } }) });
    await expect(quiet(() => runProbe(fakeOps(blocked), new Log("dashboard-check", dir), options(true, code), { sshHost: SSH_HOST }, "s1"))).rejects.toThrow("1 blockers above; nothing changed");
    // The dashboard check's pod was replaced between the dry run and the lock.
    const moved = clusterOf({ factsUnderLock: factsOf({ pods: { ...PODS, checker: "trustiot-dashboard-check-7d9-zzzzz" } }) });
    await expect(quiet(() => runProbe(fakeOps(moved), new Log("dashboard-check", dir), options(true, code), { sshHost: SSH_HOST }, "s1"))).rejects.toThrow("changed since the plan was made");
    expect([...cluster.events, ...blocked.events, ...moved.events]).toEqual(["lock", "unlock"]);
  });
});

describe("the run's log", () => {
  test("masks the ssh host from the start and the node once the facts named it, in what lib.ts notes and saves too", () => {
    const log = new ShownLog("dashboard-check", dir, SSH_HOST);
    quietSync(() => {
      log.note(`WARNING: could not read cloudflared.service: ssh: connect to host ${SSH_HOST} (${NODE_ADDRESS}) port 22: timed out`);
      log.hideNodes([NODE, ""]);
      log.note(`    error: unable to upgrade connection: node ${NODE} is not ready`);
    });
    const saved = readFileSync(log.save("plan", { node: NODE, list: [`at ${NODE_ADDRESS}`], [NODE]: 1 }), "utf8");
    const text = readFileSync(log.file, "utf8");
    for (const hidden of [SSH_HOST, NODE, NODE_ADDRESS]) {
      expect(text).not.toContain(hidden);
      expect(saved).not.toContain(hidden);
    }
    expect(text).toContain("connect to host <ssh-host>");
    expect(text).toContain("node <demo-node> is not ready");
  });
});

describe("restarting a Deployment over ssh", () => {
  const boxWith = (exitCode: number) => {
    const calls: string[][] = [];
    const log = new ShownLog("dashboard-check", dir, SSH_HOST);
    const box = new Box(SSH_HOST, log, {
      run: async (cmd) => {
        calls.push(cmd);
        return { stdout: "", stderr: exitCode === 0 ? "" : "Connection closed by remote host", exitCode };
      },
      sleep: async () => {},
    });
    return { ops: boxOps(box, options(true), log), calls };
  };

  test("runs kubectl rollout restart once as root in the demo namespace", async () => {
    const { ops, calls } = boxWith(0);
    await quiet(() => ops.restart("trustiot-identity"));
    expect(calls).toEqual([["ssh", "-o", "BatchMode=yes", SSH_HOST, "sudo k3s kubectl rollout restart deployment/trustiot-identity --namespace trustiot"]]);
  });

  test("does not retry when ssh drops, since the restart may have gone through, and says so", async () => {
    const { ops, calls } = boxWith(255);
    const error = await quiet(() => ops.restart("trustiot-identity")).catch((e: unknown) => e);
    expect(calls).toHaveLength(1);
    expect(String(error)).toContain("the restart may have gone through");
    const failed = boxWith(1);
    await expect(quiet(() => failed.ops.restart("trustiot-identity"))).rejects.toThrow("kubectl rollout restart failed (exit 1)");
    expect(failed.calls).toHaveLength(1);
  });
});

describe("resultLines", () => {
  test("a probe without a result fails as not run", () => {
    const plan = isolationPlan(TARGETS);
    const { lines, failed } = resultLines(plan, new Map());
    expect(failed).toBe(plan.length);
    expect(lines[0]).toBe("FAIL from the dashboard check's pod, its own /readyz on 127.0.0.1:8081: not run (expects HTTP 200)");
  });
});
