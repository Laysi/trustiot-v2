// The rules and the run of scripts/demo/cleanup.ts against a fake cluster, without ssh
// (Laysi/trustiot-v2#465): the inventory, nothing deleted before the confirmation, and runs again.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type CleanupOps,
  EDGE_GATEWAY,
  INVENTORY_JSONPATH,
  type NamespaceFacts,
  type ObjectFact,
  inventoryResources,
  parseInventory,
  planCleanup,
  runCleanup,
} from "./cleanup.ts";
import { type CommonOptions, Log, confirmCode } from "./lib.ts";

const top = (ref: string, managed = false): ObjectFact => ({ ref, owned: false, managed });
const owned = (ref: string): ObjectFact => ({ ref, owned: true, managed: false });
const defaults = ["configmap/kube-root-ca.crt", "configmap/istio-ca-root-cert", "serviceaccount/default"].map((ref) => top(ref));

/** The three namespaces as the demo machine had them on 2026-10-20 (abridged). */
function pocCluster(): Map<string, NamespaceFacts> {
  return new Map<string, NamespaceFacts>([
    [
      "trustiot",
      {
        name: "trustiot",
        exists: true,
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
  lockHeldBy: string | undefined;
  /** Runs once, right after the next lock: another change made while the plan was being confirmed. */
  onLock: (() => void) | undefined;

  constructor(readonly namespaces: Map<string, NamespaceFacts>) {
    this.volumes = new Set(
      [...namespaces.values()].flatMap((ns) => ns.objects.filter((o) => o.ref.startsWith("persistentvolumeclaim/")).map((o) => `${ns.name}/${o.ref.split("/")[1]}`)),
    );
  }

  async tunnel() {
    return { active: "active", pid: "4242" };
  }
  async inventory(name: string) {
    return structuredClone(this.namespaces.get(name) ?? gone(name));
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
    const workloads = [...this.namespaces.values()].flatMap((ns) =>
      ns.objects.filter((o) => o.ref.startsWith("deployment.apps/") || o.ref.startsWith("statefulset.apps/")).map((o) => `${o.ref.split("/")[0]}/${ns.name}/${o.ref.split("/")[1]}/uid`),
    );
    return { cluster: { namespaces: { names: [...this.namespaces.keys()].sort(), sha256: [...this.namespaces.keys()].sort().join(",") } }, workloads };
  }
  async deleteObject(namespace: string, ref: string) {
    this.calls.push(`delete ${namespace} ${ref}`);
    const ns = this.namespaces.get(namespace);
    if (ns === undefined) return;
    ns.objects = ns.objects.filter((o) => o.ref !== ref);
    if (ref === EDGE_GATEWAY) ns.objects = ns.objects.filter((o) => !o.ref.endsWith("/edge-istio"));
  }
  async deleteNamespace(name: string) {
    this.calls.push(`delete namespace ${name}`);
    this.namespaces.delete(name);
    for (const volume of [...this.volumes]) if (volume.startsWith(`${name}/`)) this.volumes.delete(volume);
  }
  async volumesLeft(claims: ReadonlySet<string>) {
    return [...this.volumes].filter((v) => claims.has(v)).length;
  }
  async sleep() {}

  get changes(): string[] {
    return this.calls.filter((c) => c.startsWith("delete") || c === "lock");
  }
}

describe("cleanup.ts", () => {
  let dir: string;
  let log: Log;
  const options = (overrides: Partial<CommonOptions> = {}): CommonOptions => ({ sshHost: "box", apply: false, confirm: "", lockHeld: false, ...overrides });
  const plan = () => {
    const facts = pocCluster();
    return planCleanup([facts.get("trustiot")!, facts.get("poc")!], facts.get("istio-ingress")!);
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

    test("never asks for Secret data: names, owners, the Secret type and labels only", () => {
      expect(INVENTORY_JSONPATH).not.toMatch(/\.data|stringData/);
      expect(INVENTORY_JSONPATH.match(/\{"\\t"\}/g)).toHaveLength(6);
    });

    test("names objects the way kubectl deletes them, and marks owned and managed ones", () => {
      const text = [
        "apps/v1\tDeployment\tapp\t\t\t\t",
        "v1\tPod\tapp-8b4bccb48-8t6sl\tReplicaSet\t\t\t",
        "gateway.networking.k8s.io/v1\tGateway\tedge\t\t\t\t",
        "networking.istio.io/v1\tGateway\tedge\t\t\t\t",
        "v1\tSecret\tsh.helm.release.v1.trustiot.v1\t\thelm.sh/release.v1\t\t",
        "v1\tSecret\told-token\t\tkubernetes.io/service-account-token\t\t",
        "v1\tConfigMap\ttrustiot-config\t\t\tHelm\t",
        "v1\tServiceAccount\tsource-controller\t\t\t\tflux-system",
        "helm.toolkit.fluxcd.io/v2\tHelmRelease\ttrustiot\t\t\t\t",
        "",
      ].join("\n");
      expect(parseInventory(text)).toEqual([
        { ref: "configmap/trustiot-config", owned: false, managed: true },
        { ref: "deployment.apps/app", owned: false, managed: false },
        { ref: "gateway.gateway.networking.k8s.io/edge", owned: false, managed: false },
        { ref: "gateway.networking.istio.io/edge", owned: false, managed: false },
        { ref: "helmrelease.helm.toolkit.fluxcd.io/trustiot", owned: false, managed: true },
        { ref: "pod/app-8b4bccb48-8t6sl", owned: true, managed: false },
        { ref: "secret/old-token", owned: true, managed: false },
        { ref: "secret/sh.helm.release.v1.trustiot.v1", owned: false, managed: true },
        { ref: "serviceaccount/source-controller", owned: false, managed: true },
      ]);
      expect(() => parseInventory("v1\tPod\n")).toThrow(/unexpected inventory line/);
    });
  });

  describe("plan", () => {
    test("removes both PoC namespaces and istio-ingress, naming every object that is not owned by another", () => {
      const p = plan();
      expect(p.deleteNamespaces.map((n) => n.name)).toEqual(["trustiot", "poc", "istio-ingress"]);
      expect(p.deleteNamespaces[0]?.volumeClaims).toEqual(["data-postgres-0"]);
      expect(p.deleteNamespaces[0]?.objects).toContain("secret/pg");
      expect(p.deleteNamespaces[0]?.objects).not.toContain("pod/postgres-0");
      expect(p.deleteNamespaces[1]?.objects).toContain("deployment.apps/probe-c");
      expect(p.deleteNamespaces[2]?.objects).toContain(EDGE_GATEWAY);
      expect(p.deleteNamespaces[2]?.objects).not.toContain("deployment.apps/edge-istio");
      expect(p.refused).toEqual([]);
    });

    test("leaves trustiot alone once the demo environment runs there", () => {
      const demo: NamespaceFacts = { name: "trustiot", exists: true, managed: false, objects: [...defaults, top("deployment.apps/trustiot-identity", true), top("secret/sh.helm.release.v1.trustiot.v1", true)] };
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
        { namespace: "istio-ingress", objects: ["authorizationpolicy.security.istio.io/edge-cookie-auth", "configmap/edge-options", EDGE_GATEWAY, "networkpolicy.networking.k8s.io/edge-host-only"] },
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
      const managedNamespace = planCleanup([], { ...edge, managed: true });
      expect(managedNamespace.deleteNamespaces).toEqual([]);
    });

    test("an istio-ingress without the edge PoC is not touched, even when it is empty", () => {
      const p = planCleanup([], { name: "istio-ingress", exists: true, managed: false, objects: defaults });
      expect(p.deleteNamespaces).toEqual([]);
      expect(p.deleteObjects).toEqual([]);
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

    test("--apply with the code removes the Gateway before any namespace, then istio-ingress, then the PoC namespaces, and waits for the volumes", async () => {
      const cluster = new FakeCluster(pocCluster());
      const code = confirmCode(await runCleanup(cluster, log, options()));
      await runCleanup(cluster, log, options({ apply: true, confirm: code }));
      expect(cluster.changes).toEqual([
        "lock",
        `delete istio-ingress ${EDGE_GATEWAY}`,
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
      const code = confirmCode(await runCleanup(cluster, log, options()));
      await runCleanup(cluster, log, options({ apply: true, confirm: code }));
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
      const code = confirmCode(await runCleanup(cluster, log, options()));
      await runCleanup(cluster, log, options({ apply: true, confirm: code }));
      const before = cluster.calls.length;
      for (const again of [options(), options({ apply: true, confirm: code })]) {
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

    test("a refused namespace stops the whole run, dry run included, before anything changes", async () => {
      const facts = pocCluster();
      facts.get("trustiot")!.objects.push(top("deployment.apps/trustiot-identity", true));
      const cluster = new FakeCluster(facts);
      await expect(runCleanup(cluster, log, options())).rejects.toThrow(/a person has to look first/);
      expect(cluster.changes).toEqual([]);
    });

    test("another holder of the lab lock stops --apply before anything changes", async () => {
      const cluster = new FakeCluster(pocCluster());
      const code = confirmCode(await runCleanup(cluster, log, options()));
      cluster.lockHeldBy = "Laysi/trustiot-v2#999 2026-10-20T00:00:00Z";
      await expect(runCleanup(cluster, log, options({ apply: true, confirm: code }))).rejects.toThrow(/held by/);
      expect(cluster.changes).toEqual([]);
    });
  });
});
