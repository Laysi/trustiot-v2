// The rules and the run of scripts/demo/cleanup.ts against a fake cluster, without ssh
// (Laysi/trustiot-v2#465): the inventory, nothing deleted before the confirmation, checks right
// before each deletion, cluster-scoped references, the post-checks, and runs again.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RunResult } from "../release/exec.ts";
import {
  type CleanupOps,
  type ClusterRefs,
  EDGE_GATEWAY,
  INVENTORY_JSONPATH,
  type NamespaceFacts,
  type ObjectFact,
  boxOps,
  deleteOptions,
  inventoryResources,
  parseInventory,
  planCleanup,
  runCleanup,
} from "./cleanup.ts";
import { Box, type CommonOptions, Log, confirmCode } from "./lib.ts";

const top = (ref: string, managed = false): ObjectFact => ({ ref, owned: false, managed });
const owned = (ref: string): ObjectFact => ({ ref, owned: true, managed: false });
const defaults = ["configmap/kube-root-ca.crt", "configmap/istio-ca-root-cert", "serviceaccount/default"].map((ref) => top(ref));
const noRefs = (): ClusterRefs => ({ bindings: [], callers: [] });
/** A ClusterRoleBinding for a ServiceAccount in trustiot alone. */
const otelRead = (): ClusterRefs => ({
  bindings: [{ name: "otel-read", uid: "uid-otel-read", roleRef: "ClusterRole view", subjects: [{ kind: "ServiceAccount", namespace: "trustiot", name: "otel-collector" }], managed: false }],
  callers: [],
});

/** The three namespaces as the demo machine had them on 2026-10-20 (abridged). */
function pocCluster(): Map<string, NamespaceFacts> {
  return new Map<string, NamespaceFacts>([
    [
      "trustiot",
      {
        name: "trustiot",
        exists: true,
        uid: "uid-trustiot",
        managed: false,
        objects: [
          ...defaults,
          ...["app", "grafana", "mosquitto", "otel-collector"].map((d) => top(`deployment.apps/${d}`)),
          ...["clickhouse", "postgres", "qdrant"].map((s) => top(`statefulset.apps/${s}`)),
          top("persistentvolumeclaim/data-postgres-0"),
          top("secret/pg"),
          owned("pod/postgres-0"),
        ],
      },
    ],
    [
      "poc",
      {
        name: "poc",
        exists: true,
        uid: "uid-poc",
        managed: false,
        objects: [
          ...defaults,
          ...["bridge", "consumer", "identity", "mosquitto-poc", "probe-a", "probe-b", "probe-c"].map((d) => top(`deployment.apps/${d}`)),
          top("persistentvolumeclaim/mosquitto-poc-data"),
          top("httproute.gateway.networking.k8s.io/poc-edge"),
          owned("replicaset.apps/bridge-5f677f6cbf"),
        ],
      },
    ],
    [
      "istio-ingress",
      {
        name: "istio-ingress",
        exists: true,
        uid: "uid-edge",
        managed: false,
        objects: [
          ...defaults,
          top(EDGE_GATEWAY),
          top("authorizationpolicy.security.istio.io/edge-cookie-auth"),
          top("networkpolicy.networking.k8s.io/edge-host-only"),
          top("configmap/edge-options"),
          owned("deployment.apps/edge-istio"),
          owned("service/edge-istio"),
        ],
      },
    ],
  ]);
}

const gone = (name: string): NamespaceFacts => ({ name, exists: false, managed: false, objects: [] });

/** A cluster in memory that records every call that would change something. */
class FakeCluster implements CleanupOps {
  readonly calls: string[] = [];
  volumes: Set<string>;
  refs: ClusterRefs = noRefs();
  lockHeldBy: string | undefined;
  clock = 0;
  /** Extra workloads outside the plan, as `<kind>/<namespace>/<name>/<uid>`. */
  otherWorkloads = ["Deployment/kube-system/coredns/u1"];
  /** Runs once, right after the next lock. */
  onLock: (() => void) | undefined;
  /** Runs at the end of each deleteNamespace, with the namespace's name. */
  afterDeleteNamespace: ((name: string) => void) | undefined;
  /** When set, deleteNamespace throws this instead. */
  deleteNamespaceError: Error | undefined;
  /** Runs right after each inventory returns, with the namespace's name: a change after that look. */
  afterInventory: ((name: string) => void) | undefined;
  /** Runs once, right after the next clusterRefs returns. */
  afterClusterRefs: (() => void) | undefined;
  /** When false, deleteNamespace leaves the namespace and its volumes in place (a stuck finalizer). */
  deletes = true;
  keepVolumes = false;

  constructor(readonly namespaces: Map<string, NamespaceFacts>) {
    this.volumes = new Set(
      [...namespaces.values()].flatMap((ns) => ns.objects.filter((o) => o.ref.startsWith("persistentvolumeclaim/")).map((o) => `${ns.name}/${o.ref.split("/")[1]}`)),
    );
  }

  async tunnel() {
    return { active: "active", pid: "4242", restarts: "0", started: "t0" };
  }
  async inventory(name: string) {
    const facts = structuredClone(this.namespaces.get(name) ?? gone(name));
    this.afterInventory?.(name);
    return facts;
  }
  async clusterRefs() {
    const refs = structuredClone(this.refs);
    this.afterClusterRefs?.();
    this.afterClusterRefs = undefined;
    return refs;
  }
  async lock() {
    if (this.lockHeldBy !== undefined) throw new Error(`the lab lock is held by: ${this.lockHeldBy}`);
    this.lockHeldBy = "cleanup";
    this.calls.push("lock");
    this.onLock?.();
    this.onLock = undefined;
    return async () => {
      this.lockHeldBy = undefined;
      this.calls.push("unlock");
    };
  }
  async snapshot() {
    const workloads = [
      ...this.otherWorkloads,
      ...[...this.namespaces.values()].flatMap((ns) =>
        ns.objects.filter((o) => o.ref.startsWith("deployment.apps/") || o.ref.startsWith("statefulset.apps/")).map((o) => `${o.ref.split("/")[0]}/${ns.name}/${o.ref.split("/")[1]}/uid`),
      ),
    ].sort();
    return { cluster: { namespaces: { names: [...this.namespaces.keys()].sort(), sha256: [...this.namespaces.keys()].sort().join(",") } }, workloads };
  }
  /** Runs once, after the next deleteObject. */
  afterDeleteObject: (() => void) | undefined;

  async deleteObject(namespace: string, ref: string) {
    this.calls.push(`delete ${namespace} ${ref}`);
    const ns = this.namespaces.get(namespace);
    if (ns !== undefined) {
      ns.objects = ns.objects.filter((o) => o.ref !== ref);
      if (ref === EDGE_GATEWAY) ns.objects = ns.objects.filter((o) => !o.ref.endsWith("/edge-istio"));
    }
    this.afterDeleteObject?.();
    this.afterDeleteObject = undefined;
  }
  async deleteBinding(name: string, uid: string) {
    const binding = this.refs.bindings.find((b) => b.name === name);
    if (binding !== undefined && binding.uid !== uid) throw new Error(`Precondition failed: UID in precondition: ${uid}, UID in object meta: ${binding.uid}`);
    this.calls.push(`delete clusterrolebinding ${name}`);
    this.refs.bindings = this.refs.bindings.filter((b) => b.name !== name);
  }
  async deleteNamespace(name: string, uid: string) {
    if (this.deleteNamespaceError !== undefined) throw this.deleteNamespaceError;
    const ns = this.namespaces.get(name);
    if (ns !== undefined && ns.uid !== uid) throw new Error(`Precondition failed: UID in precondition: ${uid}, UID in object meta: ${ns.uid}`);
    this.calls.push(`delete namespace ${name}`);
    if (this.deletes) this.namespaces.delete(name);
    if (!this.keepVolumes) for (const volume of [...this.volumes]) if (volume.startsWith(`${name}/`)) this.volumes.delete(volume);
    this.afterDeleteNamespace?.(name);
  }
  async volumesLeft(claims: ReadonlySet<string>) {
    return [...this.volumes].filter((v) => claims.has(v)).length;
  }
  now() {
    return this.clock;
  }
  async sleep(ms: number) {
    this.clock += ms;
  }

  get changes(): string[] {
    return this.calls.filter((c) => c.startsWith("delete") || c === "lock");
  }
}

describe("cleanup.ts", () => {
  let dir: string;
  let log: Log;
  const options = (overrides: Partial<CommonOptions> = {}): CommonOptions => ({ sshHost: "box", apply: false, confirm: "", lockHeld: false, ...overrides });
  const plan = (refs: ClusterRefs = noRefs()) => {
    const facts = pocCluster();
    return planCleanup([facts.get("trustiot")!, facts.get("poc")!], facts.get("istio-ingress")!, refs);
  };
  /** A dry run, then --apply with its code. */
  const applyAll = async (cluster: FakeCluster) => {
    const code = confirmCode(await runCleanup(cluster, log, options()));
    return runCleanup(cluster, log, options({ apply: true, confirm: code }));
  };

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), "demo-cleanup-"));
    log = new Log("test", dir);
  });
  afterEach(() => rmSync(dir, { recursive: true, force: true }));

  describe("inventory", () => {
    test("reads every kind that can be listed and deleted, without the records the cluster derives", () => {
      expect(inventoryResources("configmaps\nendpoints\nevents\nevents.events.k8s.io\nsecrets\ndeployments.apps\n\n")).toEqual(["configmaps", "deployments.apps", "secrets"]);
    });

    test("never asks for Secret data: names, owners, the Secret type, labels and the Helm annotation only", () => {
      expect(INVENTORY_JSONPATH).not.toMatch(/\.data|stringData/);
      expect(INVENTORY_JSONPATH.match(/\{"\\t"\}/g)).toHaveLength(7);
    });

    test("names objects the way kubectl deletes them, and marks owned and managed ones", () => {
      const text = [
        "apps/v1\tDeployment\tapp\t\t\t\t\t",
        "v1\tPod\tapp-8b4bccb48-8t6sl\tReplicaSet\t\t\t\t",
        "gateway.networking.k8s.io/v1\tGateway\tedge\t\t\t\t\t",
        "networking.istio.io/v1\tGateway\tedge\t\t\t\t\t",
        "v1\tSecret\tsh.helm.release.v1.trustiot.v1\t\thelm.sh/release.v1\t\t\t",
        "v1\tSecret\told-token\t\tkubernetes.io/service-account-token\t\t\t",
        "v1\tConfigMap\ttrustiot-config\t\t\tHelm\t\t",
        "v1\tConfigMap\tadopted\t\t\t\ttrustiot\t",
        "v1\tServiceAccount\tsource-controller\t\t\t\t\tflux-system",
        "helm.toolkit.fluxcd.io/v2\tHelmRelease\ttrustiot\t\t\t\t\t",
        "source.toolkit.fluxcd.io/v1\tBucket\tb\t\t\t\t\t",
        "image.toolkit.fluxcd.io/v1beta2\tImagePolicy\tp\t\t\t\t\t",
        "",
      ].join("\n");
      expect(parseInventory(text)).toEqual([
        { ref: "bucket.source.toolkit.fluxcd.io/b", owned: false, managed: true },
        { ref: "configmap/adopted", owned: false, managed: true },
        { ref: "configmap/trustiot-config", owned: false, managed: true },
        { ref: "deployment.apps/app", owned: false, managed: false },
        { ref: "gateway.gateway.networking.k8s.io/edge", owned: false, managed: false },
        { ref: "gateway.networking.istio.io/edge", owned: false, managed: false },
        { ref: "helmrelease.helm.toolkit.fluxcd.io/trustiot", owned: false, managed: true },
        { ref: "imagepolicy.image.toolkit.fluxcd.io/p", owned: false, managed: true },
        { ref: "pod/app-8b4bccb48-8t6sl", owned: true, managed: false },
        { ref: "secret/old-token", owned: true, managed: false },
        { ref: "secret/sh.helm.release.v1.trustiot.v1", owned: false, managed: true },
        { ref: "serviceaccount/source-controller", owned: false, managed: true },
      ]);
      expect(() => parseInventory("v1\tPod\n")).toThrow(/unexpected inventory line/);
    });
  });

  describe("plan", () => {
    test("removes both PoC namespaces and istio-ingress, naming every object that is not owned by another, and each namespace's uid", () => {
      const p = plan();
      expect(p.deleteNamespaces.map((n) => [n.name, n.uid])).toEqual([
        ["trustiot", "uid-trustiot"],
        ["poc", "uid-poc"],
        ["istio-ingress", "uid-edge"],
      ]);
      expect(p.deleteNamespaces[0]?.volumeClaims).toEqual(["data-postgres-0"]);
      expect(p.deleteNamespaces[0]?.objects).toContain("secret/pg");
      expect(p.deleteNamespaces[0]?.objects).not.toContain("pod/postgres-0");
      expect(p.deleteNamespaces[1]?.objects).toContain("deployment.apps/probe-c");
      expect(p.deleteNamespaces[2]?.objects).toContain(EDGE_GATEWAY);
      expect(p.deleteNamespaces[2]?.objects).not.toContain("deployment.apps/edge-istio");
      expect(p.refused).toEqual([]);
    });

    test("a namespace made again under the same name gets another confirmation code", () => {
      const facts = pocCluster();
      const again = planCleanup([{ ...facts.get("trustiot")!, uid: "uid-new" }, facts.get("poc")!], facts.get("istio-ingress")!);
      expect(confirmCode(again)).not.toBe(confirmCode(plan()));
    });

    test("leaves trustiot alone once the demo environment runs there", () => {
      const demo: NamespaceFacts = { name: "trustiot", exists: true, uid: "u", managed: false, objects: [...defaults, top("deployment.apps/trustiot-identity", true), top("secret/sh.helm.release.v1.trustiot.v1", true)] };
      const p = planCleanup([demo, gone("poc")], gone("istio-ingress"));
      expect(p.deleteNamespaces).toEqual([]);
      expect(p.refused).toEqual([]);
      expect(p.keep[0]).toMatch(/namespace trustiot: none of the PoC's workloads.*Helm or Flux manages it/);
    });

    test("refuses the PoC next to managed objects, or only part of it", () => {
      const facts = pocCluster().get("trustiot")!;
      const mixed = planCleanup([{ ...facts, objects: [...facts.objects, top("deployment.apps/trustiot-identity", true)] }], gone("istio-ingress"));
      expect(mixed.deleteNamespaces).toEqual([]);
      expect(mixed.refused[0]).toMatch(/next to objects Helm or Flux manages \(deployment\.apps\/trustiot-identity\)/);
      const fluxNamespace = planCleanup([{ ...facts, managed: true }], gone("istio-ingress"));
      expect(fluxNamespace.refused[0]).toMatch(/the namespace/);
      const partial = planCleanup([{ ...facts, objects: facts.objects.filter((o) => o.ref !== "deployment.apps/grafana") }], gone("istio-ingress"));
      expect(partial.refused[0]).toMatch(/only part of the PoC is there \(missing deployment\.apps\/grafana\)/);
    });

    test("istio-ingress with anything else in it stays; only the edge PoC's own objects go", () => {
      const edge = pocCluster().get("istio-ingress")!;
      const p = planCleanup([], { ...edge, objects: [...edge.objects, top("gateway.gateway.networking.k8s.io/other")] });
      expect(p.deleteNamespaces).toEqual([]);
      expect(p.deleteObjects).toEqual([
        { namespace: "istio-ingress", uid: "uid-edge", objects: ["authorizationpolicy.security.istio.io/edge-cookie-auth", "configmap/edge-options", EDGE_GATEWAY, "networkpolicy.networking.k8s.io/edge-host-only"] },
      ]);
      expect(p.keep[0]).toContain("gateway.gateway.networking.k8s.io/other");
    });

    test("refuses edge objects that Helm or Flux manages, and never removes istio-ingress whole while anything in it is managed", () => {
      const edge = pocCluster().get("istio-ingress")!;
      const managedGateway = planCleanup([], { ...edge, objects: edge.objects.map((o) => (o.ref === EDGE_GATEWAY ? { ...o, managed: true } : o)) });
      expect(managedGateway.deleteNamespaces).toEqual([]);
      expect(managedGateway.deleteObjects).toEqual([]);
      expect(managedGateway.refused).toEqual([expect.stringMatching(/Helm or Flux manages objects with the edge PoC's names \(gateway\.gateway\.networking\.k8s\.io\/edge\)/)]);
      const managedDefault = planCleanup([], { ...edge, objects: edge.objects.map((o) => (o.ref === "serviceaccount/default" ? { ...o, managed: true } : o)) });
      expect(managedDefault.deleteNamespaces).toEqual([]);
      expect(managedDefault.deleteObjects.map((d) => d.namespace)).toEqual(["istio-ingress"]);
      expect(planCleanup([], { ...edge, managed: true }).deleteNamespaces).toEqual([]);
    });

    test("an istio-ingress without the edge PoC is not touched, even when it is empty", () => {
      const p = planCleanup([], { name: "istio-ingress", exists: true, uid: "u", managed: false, objects: defaults });
      expect(p.deleteNamespaces).toEqual([]);
      expect(p.deleteObjects).toEqual([]);
    });
  });

  describe("cluster-scoped references", () => {
    const binding = (name: string, subjects: { kind: string; namespace?: string; name: string }[], managed = false) => ({ name, uid: `uid-${name}`, roleRef: "ClusterRole view", subjects, managed });

    test("a ClusterRoleBinding whose subjects are all in namespaces that go is listed and goes; its ClusterRole stays", () => {
      const refs = { bindings: [binding("otel-read", [{ kind: "ServiceAccount", namespace: "trustiot", name: "otel-collector" }]), binding("other", [{ kind: "ServiceAccount", namespace: "kube-system", name: "x" }])], callers: [] };
      const p = plan(refs);
      expect(p.deleteBindings).toEqual([{ name: "otel-read", uid: "uid-otel-read", roleRef: "ClusterRole view", subjects: ["ServiceAccount:trustiot/otel-collector"] }]);
      expect(p.keep).toContain("ClusterRole view stays (only its binding otel-read goes)");
      expect(confirmCode(p)).not.toBe(confirmCode(plan()));
    });

    test("one with subjects elsewhere too, or managed by Helm or Flux, is refused", () => {
      const mixed = plan({ bindings: [binding("mixed", [{ kind: "ServiceAccount", namespace: "poc", name: "probe-a" }, { kind: "User", name: "alice" }])], callers: [] });
      expect(mixed.refused).toEqual([expect.stringMatching(/ClusterRoleBinding mixed: subjects both in namespaces that go and elsewhere/)]);
      const managed = plan({ bindings: [binding("helm", [{ kind: "ServiceAccount", namespace: "poc", name: "probe-a" }], true)], callers: [] });
      expect(managed.refused).toEqual([expect.stringMatching(/ClusterRoleBinding helm: Helm or Flux manages it/)]);
    });

    test("a webhook, APIService or CRD conversion that calls into a namespace that goes is refused", () => {
      const p = plan({
        bindings: [],
        callers: [
          { kind: "ValidatingWebhookConfiguration", name: "poc-guard", namespaces: ["poc"] },
          { kind: "APIService", name: "v1.metrics.poc", namespaces: ["poc"] },
          { kind: "MutatingWebhookConfiguration", name: "istio-sidecar-injector", namespaces: ["istio-system"] },
        ],
      });
      expect(p.refused).toEqual([expect.stringMatching(/ValidatingWebhookConfiguration poc-guard calls a service in namespace poc/), expect.stringMatching(/APIService v1\.metrics\.poc/)]);
    });

    test("after an interrupted run, a binding into a PoC namespace that is already gone still goes", () => {
      const p = planCleanup([gone("trustiot"), gone("poc")], gone("istio-ingress"), { bindings: [binding("left", [{ kind: "ServiceAccount", namespace: "trustiot", name: "default" }])], callers: [] });
      expect(p.deleteBindings.map((b) => b.name)).toEqual(["left"]);
    });
  });

  describe("run", () => {
    test("a dry run only reads, and prints the code that --apply needs", async () => {
      const cluster = new FakeCluster(pocCluster());
      const shown = await runCleanup(cluster, log, options());
      expect(cluster.changes).toEqual([]);
      expect(await Bun.file(log.file).text()).toContain(`--apply --confirm ${confirmCode(shown)}`);
      expect(cluster.namespaces.size).toBe(3);
    });

    test("--apply without the code, or with another plan's code, deletes nothing and takes no lock", async () => {
      const cluster = new FakeCluster(pocCluster());
      await expect(runCleanup(cluster, log, options({ apply: true }))).rejects.toThrow(/needs --confirm/);
      await expect(runCleanup(cluster, log, options({ apply: true, confirm: "0123456789ab" }))).rejects.toThrow(/plan changed/);
      expect(cluster.changes).toEqual([]);
      expect(cluster.namespaces.size).toBe(3);
    });

    test("--apply with the code removes the Gateway before any namespace, then the bindings, istio-ingress, the PoC namespaces, and waits for the volumes", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.refs = otelRead();
      await applyAll(cluster);
      expect(cluster.changes).toEqual([
        "lock",
        `delete istio-ingress ${EDGE_GATEWAY}`,
        "delete clusterrolebinding otel-read",
        "delete namespace istio-ingress",
        "delete namespace trustiot",
        "delete namespace poc",
      ]);
      expect(cluster.calls.at(-1)).toBe("unlock");
      expect(cluster.namespaces.size).toBe(0);
      expect(cluster.volumes.size).toBe(0);
    });

    test("an istio-ingress that stays loses the Gateway first, then the edge PoC's other objects, before the PoC namespaces go", async () => {
      const facts = pocCluster();
      facts.get("istio-ingress")!.objects.push(top("gateway.gateway.networking.k8s.io/other"));
      const cluster = new FakeCluster(facts);
      await applyAll(cluster);
      expect(cluster.changes).toEqual([
        "lock",
        `delete istio-ingress ${EDGE_GATEWAY}`,
        "delete istio-ingress authorizationpolicy.security.istio.io/edge-cookie-auth",
        "delete istio-ingress configmap/edge-options",
        "delete istio-ingress networkpolicy.networking.k8s.io/edge-host-only",
        "delete namespace trustiot",
        "delete namespace poc",
      ]);
      expect(cluster.namespaces.get("istio-ingress")?.objects.map((o) => o.ref)).toContain("gateway.gateway.networking.k8s.io/other");
    });

    test("runs again: after the cleanup there is nothing to remove, and nothing changes", async () => {
      const cluster = new FakeCluster(pocCluster());
      await applyAll(cluster);
      const before = cluster.calls.length;
      for (const again of [options(), options({ apply: true, confirm: "0123456789ab" })]) {
        const p = await runCleanup(cluster, log, again);
        expect(p.deleteNamespaces).toEqual([]);
        expect(p.keep).toEqual(["namespace trustiot: already gone", "namespace poc: already gone", "namespace istio-ingress: already gone"]);
      }
      expect(cluster.calls.length).toBe(before);
    });

    test("stops under the lock, before deleting, when the cluster changed after the plan was shown", async () => {
      const cluster = new FakeCluster(pocCluster());
      const code = confirmCode(await runCleanup(cluster, log, options()));
      cluster.onLock = () => cluster.namespaces.get("poc")!.objects.push(top("deployment.apps/probe-d"));
      await expect(runCleanup(cluster, log, options({ apply: true, confirm: code }))).rejects.toThrow(/cluster changed since the plan was made/);
      expect(cluster.changes).toEqual(["lock"]);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("looks at each namespace again right before it goes, and stops when something new appeared there", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.afterDeleteNamespace = (name) => {
        if (name === "istio-ingress") cluster.namespaces.get("poc")!.objects.push(top("deployment.apps/new-thing"));
      };
      await expect(applyAll(cluster)).rejects.toThrow(/namespace poc now also holds deployment\.apps\/new-thing; stopped before deleting it/);
      expect(cluster.changes).not.toContain("delete namespace poc");
      expect(cluster.namespaces.has("poc")).toBe(true);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("reads each ClusterRoleBinding again right before it goes, and stops when a subject was added elsewhere", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.refs = otelRead();
      cluster.afterDeleteObject = () => {
        cluster.refs.bindings[0]!.subjects.push({ kind: "ServiceAccount", namespace: "monitoring", name: "agent" });
      };
      await expect(applyAll(cluster)).rejects.toThrow(/ClusterRoleBinding otel-read changed since the plan was made .*ServiceAccount:monitoring\/agent.*stopped before deleting it/);
      expect(cluster.changes).not.toContain("delete clusterrolebinding otel-read");
      expect(cluster.changes.filter((c) => c.startsWith("delete namespace"))).toEqual([]);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("an istio-ingress that stays and comes under Helm or Flux during the run keeps the edge PoC's other objects", async () => {
      const facts = pocCluster();
      facts.get("istio-ingress")!.objects.push(top("gateway.gateway.networking.k8s.io/other"));
      const cluster = new FakeCluster(facts);
      cluster.afterDeleteObject = () => {
        cluster.namespaces.get("istio-ingress")!.managed = true;
      };
      await expect(applyAll(cluster)).rejects.toThrow(/Helm or Flux now manages namespace istio-ingress; stopped before deleting anything more in it/);
      expect(cluster.changes).toEqual(["lock", `delete istio-ingress ${EDGE_GATEWAY}`]);
    });

    test("stops before deleting a namespace that was made again under the same name", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.afterDeleteNamespace = (name) => {
        if (name === "istio-ingress") cluster.namespaces.get("trustiot")!.uid = "uid-made-again";
      };
      await expect(applyAll(cluster)).rejects.toThrow(/namespace trustiot has uid uid-made-again, the plan has uid-trustiot/);
      expect(cluster.changes).not.toContain("delete namespace trustiot");
    });

    test("a namespace made again between the last look and its delete stays: the delete carries the planned uid", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.afterInventory = (name) => {
        if (name === "trustiot" && cluster.calls.includes("delete namespace istio-ingress")) {
          cluster.namespaces.get("trustiot")!.uid = "uid-made-again";
          cluster.afterInventory = undefined;
        }
      };
      await expect(applyAll(cluster)).rejects.toThrow("Precondition failed: UID in precondition: uid-trustiot, UID in object meta: uid-made-again");
      expect(cluster.namespaces.get("trustiot")?.uid).toBe("uid-made-again");
      expect(cluster.changes).not.toContain("delete namespace trustiot");
      expect(cluster.changes).not.toContain("delete namespace poc");
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("a ClusterRoleBinding with another uid when it is read again stops the run before it goes", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.refs = otelRead();
      cluster.afterDeleteObject = () => {
        cluster.refs.bindings[0]!.uid = "uid-made-again";
      };
      await expect(applyAll(cluster)).rejects.toThrow(/ClusterRoleBinding otel-read changed since the plan was made \(now uid uid-made-again,/);
      expect(cluster.changes).not.toContain("delete clusterrolebinding otel-read");
      expect(cluster.changes.filter((c) => c.startsWith("delete namespace"))).toEqual([]);
    });

    test("a ClusterRoleBinding made again between that read and its delete stays: the delete carries the planned uid", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.refs = otelRead();
      cluster.afterDeleteObject = () => {
        // The next clusterRefs is the read right before the binding goes.
        cluster.afterClusterRefs = () => {
          cluster.refs.bindings[0]!.uid = "uid-made-again";
        };
      };
      await expect(applyAll(cluster)).rejects.toThrow("Precondition failed: UID in precondition: uid-otel-read, UID in object meta: uid-made-again");
      expect(cluster.refs.bindings.map((b) => b.uid)).toEqual(["uid-made-again"]);
      expect(cluster.changes.filter((c) => c.startsWith("delete namespace"))).toEqual([]);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("a binding or webhook pointing into a namespace after the plan stops the run before that namespace goes", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.afterDeleteNamespace = (name) => {
        if (name === "istio-ingress") {
          cluster.refs.bindings.push({ name: "late", uid: "uid-late", roleRef: "ClusterRole admin", subjects: [{ kind: "ServiceAccount", namespace: "trustiot", name: "default" }], managed: false });
        }
      };
      await expect(applyAll(cluster)).rejects.toThrow("ClusterRoleBinding late now points into namespace trustiot, not in the plan; stopped before deleting it");
      expect(cluster.changes).toEqual(["lock", `delete istio-ingress ${EDGE_GATEWAY}`, "delete namespace istio-ingress"]);
      expect(cluster.calls.at(-1)).toBe("unlock");

      const webhook = new FakeCluster(pocCluster());
      webhook.afterDeleteNamespace = (name) => {
        if (name === "trustiot") webhook.refs.callers.push({ kind: "MutatingWebhookConfiguration", name: "late", namespaces: ["poc"] });
      };
      await expect(applyAll(webhook)).rejects.toThrow("MutatingWebhookConfiguration late now points into namespace poc, not in the plan; stopped before deleting it");
      expect(webhook.changes).not.toContain("delete namespace poc");
    });

    test("the records directory is printed with the home directory as ~", async () => {
      const home = process.env.HOME;
      const xdg = process.env.XDG_STATE_HOME;
      process.env.HOME = dir;
      delete process.env.XDG_STATE_HOME;
      try {
        log = new Log("test");
        await applyAll(new FakeCluster(pocCluster()));
        const printed = await Bun.file(log.file).text();
        expect(printed).toContain("done; records in ~/.local/state/trustiot-v2/demo\n");
        expect(printed).not.toContain(dir);
      } finally {
        process.env.HOME = home;
        if (xdg === undefined) delete process.env.XDG_STATE_HOME;
        else process.env.XDG_STATE_HOME = xdg;
      }
    });

    test("a refused namespace or reference stops the whole run, dry run included, before anything changes", async () => {
      const facts = pocCluster();
      facts.get("trustiot")!.objects.push(top("deployment.apps/trustiot-identity", true));
      const cluster = new FakeCluster(facts);
      await expect(runCleanup(cluster, log, options())).rejects.toThrow(/a person has to look first/);
      const webhook = new FakeCluster(pocCluster());
      webhook.refs = { bindings: [], callers: [{ kind: "ValidatingWebhookConfiguration", name: "poc-guard", namespaces: ["poc"] }] };
      await expect(runCleanup(webhook, log, options())).rejects.toThrow(/poc-guard calls a service in namespace poc/);
      expect([...cluster.changes, ...webhook.changes]).toEqual([]);
    });

    test("another holder of the lab lock stops --apply before anything changes", async () => {
      const cluster = new FakeCluster(pocCluster());
      const code = confirmCode(await runCleanup(cluster, log, options()));
      cluster.lockHeldBy = "Laysi/trustiot-v2#999 2026-10-20T00:00:00Z";
      await expect(runCleanup(cluster, log, options({ apply: true, confirm: code }))).rejects.toThrow(/held by/);
      expect(cluster.changes).toEqual([]);
    });

    test("a failure in the middle of the run still releases the lock and reports the ops tunnel", async () => {
      const cluster = new FakeCluster(pocCluster());
      const code = confirmCode(await runCleanup(cluster, log, options()));
      cluster.deleteNamespaceError = new Error("connection refused");
      await expect(runCleanup(cluster, log, options({ apply: true, confirm: code }))).rejects.toThrow("connection refused");
      expect(cluster.calls.at(-1)).toBe("unlock");
      expect((await Bun.file(log.file).text()).trimEnd().split("\n").at(-1)).toContain("cloudflared.service kept running (same process)");
    });
  });

  describe("post-checks", () => {
    test("a workload outside the plan that went during the run fails the run", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.afterDeleteNamespace = () => {
        cluster.otherWorkloads = [];
      };
      await expect(applyAll(cluster)).rejects.toThrow(/workloads outside the plan changed during the run: Deployment\/kube-system\/coredns\/u1/);
    });

    test("a workload that appeared during the run fails the run", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.afterDeleteNamespace = () => {
        cluster.otherWorkloads = ["Deployment/kube-system/coredns/u1", "Deployment/default/surprise/u2"];
      };
      await expect(applyAll(cluster)).rejects.toThrow(/Deployment\/default\/surprise\/u2/);
    });

    test("a plan that is not empty after the deletions fails the run", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.deletes = false;
      cluster.afterDeleteNamespace = (name) => {
        // The namespace did not go, but nothing new is in it either: only the final re-plan sees it.
        void name;
      };
      await expect(applyAll(cluster)).rejects.toThrow(/after the run the plan is not empty/);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });

    test("volumes that never go time out after 300 s", async () => {
      const cluster = new FakeCluster(pocCluster());
      cluster.keepVolumes = true;
      await expect(applyAll(cluster)).rejects.toThrow(/timed out after 300 s waiting for the persistent volumes of the removed claims \(2 left\)/);
      // One 5 s poll past the 300 s deadline, not more.
      expect(cluster.clock).toBe(305_000);
      expect(cluster.calls.at(-1)).toBe("unlock");
    });
  });

  describe("boxOps over ssh", () => {
    /** A fake demo machine that answers the kubectl calls boxOps makes. */
    function fakeBox(answer: (command: string) => RunResult | undefined) {
      const commands: string[] = [];
      const stdins: (string | undefined)[] = [];
      const run = async (cmd: string[], stdin?: string | Uint8Array): Promise<RunResult> => {
        const command = cmd.at(-1)!;
        commands.push(command);
        stdins.push(stdin === undefined ? undefined : String(stdin));
        if (command.startsWith("mkdir ")) return { stdout: "ACQUIRED", stderr: "", exitCode: 0 };
        return answer(command) ?? { stdout: "", stderr: "unexpected", exitCode: 1 };
      };
      return { box: new Box("demo-box", log, { run, sleep: async () => {} }), commands, stdins };
    }
    const namespaceJson = (name: string) => ({ stdout: JSON.stringify({ metadata: { name, uid: `uid-${name}` } }), stderr: "", exitCode: 0 });
    const PLANNED = "0b6d7a2e-1c3f-4d5e-8f90-a1b2c3d4e5f6";
    const OTHER = "7c1e0f9a-2b3d-4e5f-9a8b-c7d6e5f4a3b2";
    const object = (metadata: Record<string, unknown>) => ({ stdout: JSON.stringify({ metadata }), stderr: "", exitCode: 0 });
    const notFound = { stdout: "", stderr: 'Error from server (NotFound): namespaces "poc" not found', exitCode: 1 };
    const conflict = (message: string) => ({ stdout: "", stderr: `Error from server (Conflict): ${message}`, exitCode: 1 });
    /** A clock that moves with each sleep, so a wait that should not happen ends at its deadline. */
    function ticking() {
      let now = 0;
      return { now: () => now, sleep: async (ms: number) => void (now += ms) };
    }
    /** Answers the namespace gets in turn, the last one again and again. */
    function sequence(answers: RunResult[]): () => RunResult {
      let next = 0;
      return () => answers[Math.min(next++, answers.length - 1)]!;
    }
    const resources = { stdout: "configmaps\nendpoints\nevents\nevents.events.k8s.io\ndeployments.apps\n", stderr: "", exitCode: 0 };

    test("the inventory lists only the filtered kinds, and stops when kubectl fails", async () => {
      const { box, commands } = fakeBox((c) => (c.includes("get namespace poc") ? namespaceJson("poc") : c.includes("api-resources") ? resources : undefined));
      await expect(boxOps(box, options()).inventory("poc")).rejects.toThrow(/kubectl -n poc get failed \(exit 1\)/);
      const get = commands.find((c) => c.includes("-n poc get"))!;
      expect(get).toContain(" configmaps,deployments.apps ");
    });

    test("the kinds are read again under the lock", async () => {
      const { box, commands } = fakeBox((c) =>
        c.includes("get namespace poc") ? namespaceJson("poc") : c.includes("api-resources") ? resources : c.includes("-n poc get") ? { stdout: "", stderr: "", exitCode: 0 } : undefined,
      );
      const ops = boxOps(box, options());
      await ops.inventory("poc");
      await ops.inventory("poc");
      expect(commands.filter((c) => c.includes("api-resources"))).toHaveLength(1);
      await ops.lock();
      await ops.inventory("poc");
      expect(commands.filter((c) => c.includes("api-resources"))).toHaveLength(2);
    });

    test("a namespace that never goes times out after 600 s, polling with short commands", async () => {
      const { box, commands } = fakeBox((c) =>
        c.includes("get namespace stuck") ? object({ name: "stuck", uid: PLANNED, deletionTimestamp: "2026-10-20T00:00:00Z" }) : c.includes("delete --raw") ? { stdout: "", stderr: "", exitCode: 0 } : undefined,
      );
      let now = 0;
      const ops = boxOps(box, options(), { now: () => now, sleep: async (ms) => void (now += ms) });
      await expect(ops.deleteNamespace("stuck", PLANNED)).rejects.toThrow(/timed out after 600 s waiting for namespace stuck/);
      expect(now).toBe(605_000);
      expect(commands.filter((c) => c.includes("delete"))).toEqual(["sudo k3s kubectl delete --raw /api/v1/namespaces/stuck -f -"]);
    });

    test("a namespace is deleted with its planned uid as a precondition, sent as the DELETE's body", async () => {
      const gets = sequence([object({ name: "poc", uid: PLANNED, deletionTimestamp: "2026-10-20T00:00:00Z" }), notFound]);
      const { box, commands, stdins } = fakeBox((c) => (c.includes("get namespace poc") ? gets() : c.includes("delete --raw") ? { stdout: "", stderr: "", exitCode: 0 } : undefined));
      await boxOps(box, options(), ticking()).deleteNamespace("poc", PLANNED);
      const index = commands.indexOf("sudo k3s kubectl delete --raw /api/v1/namespaces/poc -f -");
      expect(index).toBeGreaterThanOrEqual(0);
      expect(JSON.parse(stdins[index]!)).toEqual({ kind: "DeleteOptions", apiVersion: "v1", preconditions: { uid: PLANNED }, propagationPolicy: "Background" });
    });

    test("a namespace made again under the same name is left alone, and a repeated delete of one already going is fine", async () => {
      const remade = fakeBox((c) =>
        c.includes("get namespace poc") ? object({ name: "poc", uid: OTHER }) : c.includes("delete --raw") ? conflict(`Precondition failed: UID in precondition: ${PLANNED}, UID in object meta: ${OTHER}`) : undefined,
      );
      await expect(boxOps(remade.box, options(), ticking()).deleteNamespace("poc", PLANNED)).rejects.toThrow(
        `namespace poc now has uid ${OTHER}, the plan has ${PLANNED}: it was made again, and the new one stays`,
      );
      const gets = sequence([object({ name: "poc", uid: PLANNED, deletionTimestamp: "2026-10-20T00:00:00Z" }), notFound]);
      const going = fakeBox((c) => (c.includes("get namespace poc") ? gets() : c.includes("delete --raw") ? conflict("The system is ensuring all content is removed from this namespace.") : undefined));
      await boxOps(going.box, options(), ticking()).deleteNamespace("poc", PLANNED);
      const refused = fakeBox((c) => (c.includes("get namespace poc") ? object({ name: "poc", uid: PLANNED }) : c.includes("delete --raw") ? { stdout: "", stderr: "Forbidden", exitCode: 1 } : undefined));
      await expect(boxOps(refused.box, options(), ticking()).deleteNamespace("poc", PLANNED)).rejects.toThrow(
        /deleting namespace poc with uid .* failed \(exit 1\): Forbidden/,
      );
    });

    test("a ClusterRoleBinding is deleted with its uid as a precondition, and one made again stays", async () => {
      const { box, commands, stdins } = fakeBox((c) => (c.includes("delete --raw") ? { stdout: "", stderr: "", exitCode: 0 } : undefined));
      await boxOps(box, options()).deleteBinding("system:otel-read", PLANNED);
      expect(commands).toEqual(["sudo k3s kubectl delete --raw /apis/rbac.authorization.k8s.io/v1/clusterrolebindings/system%3Aotel-read -f -"]);
      expect(JSON.parse(stdins[0]!)).toEqual({ kind: "DeleteOptions", apiVersion: "v1", preconditions: { uid: PLANNED }, propagationPolicy: "Background" });
      const remade = fakeBox((c) => (c.includes("get clusterrolebindings") ? object({ name: "otel-read", uid: OTHER }) : c.includes("delete --raw") ? conflict("Precondition failed") : undefined));
      await expect(boxOps(remade.box, options()).deleteBinding("otel-read", PLANNED)).rejects.toThrow(`with uid ${OTHER}, made again since the plan, and stays`);
      const goneAlready = fakeBox((c) =>
        c.includes("get clusterrolebindings")
          ? { stdout: "", stderr: 'Error from server (NotFound): clusterrolebindings.rbac.authorization.k8s.io "otel-read" not found', exitCode: 1 }
          : c.includes("delete --raw")
            ? notFound
            : undefined,
      );
      await boxOps(goneAlready.box, options()).deleteBinding("otel-read", PLANNED);
    });

    test("a delete without a valid uid or name sends nothing", async () => {
      expect(() => deleteOptions("")).toThrow(/not a Kubernetes uid/);
      expect(() => deleteOptions("uid-poc")).toThrow(/not a Kubernetes uid/);
      const { box, commands } = fakeBox(() => undefined);
      await expect(boxOps(box, options()).deleteNamespace("poc", "")).rejects.toThrow(/not a Kubernetes uid/);
      await expect(boxOps(box, options()).deleteBinding("otel-read", "")).rejects.toThrow(/not a Kubernetes uid/);
      await expect(boxOps(box, options()).deleteNamespace("../x", PLANNED)).rejects.toThrow(/not a namespace name/);
      await expect(boxOps(box, options()).deleteBinding("..", PLANNED)).rejects.toThrow(/not an object name/);
      expect(commands).toEqual([]);
    });
  });
});
