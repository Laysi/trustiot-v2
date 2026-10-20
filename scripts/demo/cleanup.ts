/**
 * Removes the 2026-10-09/10 feasibility leftovers from the demo machine, as decided on 2026-10-20
 * (Laysi/trustiot-v2#465, part of the demo environment Laysi/trustiot-v2#457; the results of the
 * feasibility tests are kept in docs/feasibility/):
 *
 * - namespace `trustiot` of the cloud PoC (app, clickhouse, grafana, mosquitto, otel-collector,
 *   postgres, qdrant; its local-path volumes and their data go with it): the demo environment
 *   uses that name;
 * - namespace `poc` of the edge PoC (its probes included);
 * - the edge PoC's Gateway `edge` in `istio-ingress` and its objects, and that namespace when
 *   nothing else is in it.
 *
 * The istiod ext_authz provider that points at the PoC is replaced by platform.ts, after this.
 * Nothing on the demo machine's file system is touched. Usage (scripts/README.md, 「展示機」):
 *
 *   bun scripts/demo/cleanup.ts --ssh-host <host>                       # plan only, read only
 *   bun scripts/demo/cleanup.ts --ssh-host <host> --apply --confirm <code>
 *
 * The plan names every object that goes (objects that another object owns, such as Pods and
 * ReplicaSets, go with their owner and are not listed). A namespace is only removed while it still
 * looks like the PoC: all of the PoC's workloads are there and nothing in it is managed by Helm or
 * Flux. So once the demo environment runs in `trustiot`, this script leaves it alone. Anything
 * ambiguous (some of the PoC's workloads, or the PoC's next to Helm's) stops the run before
 * anything changes. See lib.ts for the lock, the confirmation code and the records.
 */

import { parseOptions } from "../release/exec.ts";
import { ReleaseError } from "../release/lib.ts";
import {
  Box,
  COMMON_SPEC,
  type CommonOptions,
  DEMO_NAMESPACE,
  type KindState,
  Log,
  type OpsTunnel,
  POC_FINGERPRINTS,
  checkConfirmation,
  clusterState,
  commonOptions,
  main,
  opsTunnel,
  opsTunnelUnchanged,
  requireOpsTunnel,
  stableJson,
  stateDiff,
  takeLock,
  workloads,
} from "./lib.ts";

export const EDGE_NAMESPACE = "istio-ingress";
/** The edge PoC's Gateway, which Istio made a Deployment, Service and ServiceAccount for. */
export const EDGE_GATEWAY = "gateway.gateway.networking.k8s.io/edge";
/** The edge PoC's own objects in istio-ingress; with the namespace's defaults, all it may hold to be removed whole. */
export const EDGE_OBJECTS = [
  EDGE_GATEWAY,
  "authorizationpolicy.security.istio.io/edge-cookie-auth",
  "networkpolicy.networking.k8s.io/edge-host-only",
  "configmap/edge-options",
  // Istio makes these for the Gateway and deletes them with it (owner references); listed in case
  // an older Istio left them without one.
  "deployment.apps/edge-istio",
  "service/edge-istio",
  "serviceaccount/edge-istio",
] as const;
/** What Kubernetes and Istio put into every namespace. */
export const NAMESPACE_DEFAULTS = ["configmap/kube-root-ca.crt", "configmap/istio-ca-root-cert", "configmap/istio-ca-crl", "serviceaccount/default"] as const;
/** Kinds left out of the inventory: records that the cluster derives and removes by itself. */
const DERIVED_RESOURCES = new Set(["events", "events.events.k8s.io", "endpoints"]);

/** One object in a namespace, as the inventory found it. */
export interface ObjectFact {
  /** `kind.group/name` (lower-case kind, no group for the core API): what kubectl deletes. */
  ref: string;
  /** Another object owns it (a Pod of a ReplicaSet, the Gateway's Deployment): it goes with its owner. */
  owned: boolean;
  /** Helm or Flux manages it: a Helm label, a Helm release record, a Flux label or a Flux object. */
  managed: boolean;
}

export interface NamespaceFacts {
  name: string;
  exists: boolean;
  /** The namespace itself carries a Helm or Flux label. */
  managed: boolean;
  objects: ObjectFact[];
}

export interface CleanupPlan {
  /** Namespaces that go, with every object in them that is not owned by another one. */
  deleteNamespaces: { name: string; objects: string[]; volumeClaims: string[] }[];
  /** Objects that go from a namespace that stays. */
  deleteObjects: { namespace: string; objects: string[] }[];
  keep: string[];
  /** Namespaces that need a person: the run stops before anything changes. */
  refused: string[];
}

/** The ref of a Deployment or StatefulSet in POC_FINGERPRINTS. */
function fingerprintRefs(namespace: string): string[] {
  const fingerprint = POC_FINGERPRINTS[namespace];
  if (fingerprint === undefined) throw new ReleaseError(`no PoC fingerprint for namespace ${namespace}`);
  return [...fingerprint.deployments.map((d) => `deployment.apps/${d}`), ...fingerprint.statefulsets.map((s) => `statefulset.apps/${s}`)];
}

/**
 * What goes, from what is there. Pure, so the rules are tested without a cluster:
 * - a PoC namespace goes only with its whole fingerprint present and nothing Helm or Flux manages;
 * - one without any of the PoC's workloads is not the PoC (the demo environment, later) and stays;
 * - anything in between is refused.
 * istio-ingress goes whole when it holds only the edge PoC's objects and the namespace defaults;
 * otherwise only the edge PoC's own objects go.
 */
export function planCleanup(namespaces: readonly NamespaceFacts[], edge: NamespaceFacts): CleanupPlan {
  const plan: CleanupPlan = { deleteNamespaces: [], deleteObjects: [], keep: [], refused: [] };
  for (const ns of namespaces) {
    const fingerprint = fingerprintRefs(ns.name);
    if (!ns.exists) {
      plan.keep.push(`namespace ${ns.name}: already gone`);
      continue;
    }
    const refs = new Set(ns.objects.map((o) => o.ref));
    const present = fingerprint.filter((ref) => refs.has(ref));
    const missing = fingerprint.filter((ref) => !refs.has(ref));
    const managed = [...(ns.managed ? ["the namespace"] : []), ...ns.objects.filter((o) => o.managed).map((o) => o.ref)];
    if (present.length === 0) {
      plan.keep.push(`namespace ${ns.name}: none of the PoC's workloads are in it${managed.length > 0 ? " (Helm or Flux manages it)" : ""}; not the PoC`);
    } else if (managed.length > 0) {
      plan.refused.push(`namespace ${ns.name}: the PoC's ${present.join(", ")} next to objects Helm or Flux manages (${managed.slice(0, 5).join(", ")}${managed.length > 5 ? ", ..." : ""})`);
    } else if (missing.length > 0) {
      plan.refused.push(`namespace ${ns.name}: only part of the PoC is there (missing ${missing.join(", ")})`);
    } else {
      plan.deleteNamespaces.push({
        name: ns.name,
        objects: topLevel(ns),
        volumeClaims: topLevel(ns)
          .filter((ref) => ref.startsWith("persistentvolumeclaim/"))
          .map((ref) => ref.slice("persistentvolumeclaim/".length)),
      });
    }
  }

  if (!edge.exists) {
    plan.keep.push(`namespace ${EDGE_NAMESPACE}: already gone`);
    return plan;
  }
  const edgeRefs = topLevel(edge);
  const ours = edgeRefs.filter((ref) => (EDGE_OBJECTS as readonly string[]).includes(ref));
  const others = edgeRefs.filter((ref) => !(EDGE_OBJECTS as readonly string[]).includes(ref) && !(NAMESPACE_DEFAULTS as readonly string[]).includes(ref));
  const managed = edge.objects.filter((o) => o.managed).map((o) => o.ref);
  const oursManaged = ours.filter((ref) => managed.includes(ref));
  if (ours.length === 0) {
    plan.keep.push(`namespace ${EDGE_NAMESPACE}: none of the edge PoC's objects are in it`);
  } else if (oursManaged.length > 0) {
    plan.refused.push(`namespace ${EDGE_NAMESPACE}: Helm or Flux manages objects with the edge PoC's names (${oursManaged.join(", ")})`);
  } else if (others.length === 0 && managed.length === 0 && !edge.managed && edgeRefs.includes(EDGE_GATEWAY)) {
    plan.deleteNamespaces.push({ name: EDGE_NAMESPACE, objects: edgeRefs, volumeClaims: [] });
  } else {
    plan.deleteObjects.push({ namespace: EDGE_NAMESPACE, objects: ours });
    plan.keep.push(`namespace ${EDGE_NAMESPACE}: also holds ${others.join(", ") || "Helm- or Flux-managed objects"}; only the edge PoC's objects go`);
  }
  return plan;
}

function topLevel(ns: NamespaceFacts): string[] {
  return ns.objects
    .filter((o) => !o.owned)
    .map((o) => o.ref)
    .sort();
}

/** True when the plan changes nothing. */
export function planIsEmpty(plan: CleanupPlan): boolean {
  return plan.deleteNamespaces.length === 0 && plan.deleteObjects.length === 0;
}

// --- Inventory ---------------------------------------------------------------------------------

/**
 * One line per object: apiVersion, kind, name, owner kinds, Secret type, Helm label, Flux labels.
 * jsonpath, never `-o json`: the inventory reads Secrets, and their data must not leave the box.
 */
export const INVENTORY_JSONPATH = [
  '{range .items[*]}',
  '{.apiVersion}{"\\t"}{.kind}{"\\t"}{.metadata.name}{"\\t"}{.metadata.ownerReferences[*].kind}{"\\t"}{.type}{"\\t"}',
  '{.metadata.labels.app\\.kubernetes\\.io/managed-by}{"\\t"}',
  '{.metadata.labels.helm\\.toolkit\\.fluxcd\\.io/name}{.metadata.labels.kustomize\\.toolkit\\.fluxcd\\.io/name}',
  '{"\\n"}{end}',
].join("");

/** Flux's own objects: one in a namespace means Flux manages something there. */
const FLUX_KINDS = new Set(["HelmRelease", "Kustomization", "GitRepository", "OCIRepository", "HelmRepository", "HelmChart"]);

/** The resource list for the inventory: every namespaced kind that can be listed and deleted. */
export function inventoryResources(apiResources: string): string[] {
  return apiResources
    .split("\n")
    .map((line) => line.trim())
    .filter((name) => name !== "" && !DERIVED_RESOURCES.has(name))
    .sort();
}

/** Parses the inventory lines (INVENTORY_JSONPATH) into object facts. */
export function parseInventory(text: string): ObjectFact[] {
  const objects: ObjectFact[] = [];
  for (const line of text.split("\n")) {
    if (line.trim() === "") continue;
    const fields = line.split("\t");
    if (fields.length !== 7) throw new ReleaseError(`unexpected inventory line (${fields.length} fields): ${line.slice(0, 120)}`);
    const [apiVersion, kind, name, owners, type, helmLabel, fluxLabel] = fields as [string, string, string, string, string, string, string];
    if (kind === "" || name === "") throw new ReleaseError(`inventory line without kind or name: ${line.slice(0, 120)}`);
    const group = apiVersion.includes("/") ? apiVersion.slice(0, apiVersion.indexOf("/")) : "";
    const isFlux = group.endsWith(".toolkit.fluxcd.io") && FLUX_KINDS.has(kind);
    objects.push({
      ref: `${kind.toLowerCase()}${group === "" ? "" : `.${group}`}/${name}`,
      // A legacy service account token goes with its ServiceAccount, like an owned object.
      owned: owners.trim() !== "" || type === "kubernetes.io/service-account-token",
      managed: helmLabel === "Helm" || fluxLabel !== "" || type.startsWith("helm.sh/release") || isFlux,
    });
  }
  return objects.sort((a, b) => a.ref.localeCompare(b.ref));
}

function labelsManaged(labels: Record<string, string>): boolean {
  return labels["app.kubernetes.io/managed-by"] === "Helm" || Object.keys(labels).some((key) => key.endsWith(".toolkit.fluxcd.io/name"));
}

// --- The run -----------------------------------------------------------------------------------

/** What the run does to the cluster; boxOps does it over ssh, the tests with a fake cluster. */
export interface CleanupOps {
  tunnel(): Promise<OpsTunnel>;
  inventory(namespace: string): Promise<NamespaceFacts>;
  lock(): Promise<() => Promise<void>>;
  snapshot(): Promise<{ cluster: Record<string, KindState>; workloads: string[] }>;
  deleteObject(namespace: string, ref: string): Promise<void>;
  deleteNamespace(name: string): Promise<void>;
  /** Persistent volumes still bound to one of these `<namespace>/<claim>`. */
  volumesLeft(claims: ReadonlySet<string>): Promise<number>;
  sleep(ms: number): Promise<void>;
}

const VOLUME_WAIT_MS = 300_000;
const NAMESPACE_WAIT_MS = 600_000;

async function currentPlan(ops: CleanupOps): Promise<CleanupPlan> {
  const namespaces = [];
  for (const name of [DEMO_NAMESPACE, "poc"]) namespaces.push(await ops.inventory(name));
  return planCleanup(namespaces, await ops.inventory(EDGE_NAMESPACE));
}

/**
 * The whole run. Without --apply it only reads. With --apply it checks the confirmation code
 * before it takes the lock or changes anything, re-plans under the lock and stops when the plan
 * changed, deletes, waits for the volumes, and checks that nothing outside the plan changed.
 * Returns the plan it worked from.
 */
export async function runCleanup(ops: CleanupOps, log: Log, options: CommonOptions): Promise<CleanupPlan> {
  const tunnelBefore = await ops.tunnel();
  requireOpsTunnel(tunnelBefore);
  const plan = await currentPlan(ops);
  log.note(`plan:\n${JSON.stringify(plan, null, 2)}`);
  log.save("plan", plan);
  if (plan.refused.length > 0) {
    throw new ReleaseError(`a person has to look first, nothing changed: ${plan.refused.join("; ")}`);
  }
  if (planIsEmpty(plan)) {
    log.note("nothing to remove: the PoC's leftovers are gone");
    return plan;
  }
  if (!checkConfirmation(log, plan, options, "bun scripts/demo/cleanup.ts --ssh-host <host>")) return plan;

  const release = await ops.lock();
  try {
    if (stableJson(await currentPlan(ops)) !== stableJson(plan)) {
      throw new ReleaseError("the cluster changed since the plan was made; nothing changed, run again without --apply");
    }
    const before = await ops.snapshot();
    log.save("state-before", before);
    // The edge Gateway before anything else: its CUSTOM policy calls the PoC's identity service in
    // `poc`, so the Gateway stops taking requests before that service goes. Istio then removes the
    // Deployment and Service it made for the Gateway.
    if ([...plan.deleteObjects.flatMap((d) => d.objects), ...plan.deleteNamespaces.flatMap((ns) => ns.objects)].includes(EDGE_GATEWAY)) {
      await ops.deleteObject(EDGE_NAMESPACE, EDGE_GATEWAY);
      log.note(`deleted ${EDGE_GATEWAY} in ${EDGE_NAMESPACE}`);
    }
    for (const { namespace, objects } of plan.deleteObjects) {
      for (const ref of objects.filter((o) => o !== EDGE_GATEWAY)) {
        await ops.deleteObject(namespace, ref);
        log.note(`deleted ${ref} in ${namespace}`);
      }
    }
    // istio-ingress first, then the namespaces whose services it used.
    for (const ns of [...plan.deleteNamespaces].sort((a, b) => Number(b.name === EDGE_NAMESPACE) - Number(a.name === EDGE_NAMESPACE))) {
      log.note(`deleting namespace ${ns.name} (${ns.objects.length} objects; volume claims: ${ns.volumeClaims.join(", ") || "none"})`);
      await ops.deleteNamespace(ns.name);
      log.note(`deleted namespace ${ns.name}`);
    }
    // local-path removes a volume and its directory with a helper pod after the claim is gone.
    const claims = new Set(plan.deleteNamespaces.flatMap((ns) => ns.volumeClaims.map((claim) => `${ns.name}/${claim}`)));
    if (claims.size > 0) {
      const deadline = Date.now() + VOLUME_WAIT_MS;
      for (;;) {
        const left = await ops.volumesLeft(claims);
        if (left === 0) break;
        if (Date.now() > deadline) {
          throw new ReleaseError(`timed out after ${VOLUME_WAIT_MS / 1000} s waiting for the persistent volumes of the removed claims (${left} left); check the local-path provisioner in kube-system`);
        }
        await ops.sleep(5_000);
      }
      log.note(`the persistent volumes of the ${claims.size} removed claims are deleted`);
    }
    const after = await ops.snapshot();
    log.save("state-after", after);
    const diff = stateDiff(before.cluster, after.cluster);
    log.note(`cluster-scoped changes: ${JSON.stringify(diff)}`);
    const removedNamespaces = new Set(plan.deleteNamespaces.map((ns) => ns.name));
    const removed = before.workloads.filter((w) => !after.workloads.includes(w));
    const added = after.workloads.filter((w) => !before.workloads.includes(w));
    // Workloads are `<kind>/<namespace>/<name>/<uid>`; the Gateway's own go with it.
    const expected = (w: string) => {
      const [, namespace, name] = w.split("/");
      return removedNamespaces.has(namespace ?? "") || (plan.deleteObjects.some((d) => d.namespace === namespace) && name === "edge-istio");
    };
    const unexpected = removed.filter((w) => !expected(w));
    log.note(`workloads and services removed: ${removed.length} (outside the plan: ${unexpected.length}); added: ${added.length}`);
    const left = await currentPlan(ops);
    if (!planIsEmpty(left)) throw new ReleaseError(`after the run the plan is not empty: ${JSON.stringify(left)}`);
    if (unexpected.length > 0 || added.length > 0) {
      throw new ReleaseError(`workloads outside the plan changed during the run: ${[...unexpected, ...added].join(", ")}`);
    }
  } finally {
    await release();
  }
  log.note(opsTunnelUnchanged(tunnelBefore, await ops.tunnel()));
  log.note(`done; records in ${log.dir}`);
  return plan;
}

/** CleanupOps on the demo machine, over ssh. */
export function boxOps(box: Box, options: CommonOptions): CleanupOps {
  let resources: string[] | undefined;
  return {
    tunnel: () => opsTunnel(box),
    async inventory(name) {
      const ns = await box.getJson(["get", "namespace", name]);
      if (ns === undefined) return { name, exists: false, managed: false, objects: [] };
      resources ??= inventoryResources(await box.kubectl(["api-resources", "--namespaced", "--verbs=list,delete", "-o", "name"]));
      // One call for every kind; kubectl exits non-zero when any kind fails, so an unreadable kind
      // stops the run instead of making the namespace look empty.
      const text = await box.kubectl(["-n", name, "get", resources.join(","), "--ignore-not-found", "-o", `jsonpath=${INVENTORY_JSONPATH}`]);
      return { name, exists: true, managed: labelsManaged((ns.metadata as { labels?: Record<string, string> }).labels ?? {}), objects: parseInventory(text) };
    },
    lock: () => takeLock(box, "cleanup", options.lockHeld),
    snapshot: async () => ({ cluster: await clusterState(box), workloads: await workloads(box) }),
    async deleteObject(namespace, ref) {
      await box.kubectl(["-n", namespace, "delete", ref, "--ignore-not-found", "--wait", "--timeout=180s"]);
    },
    async deleteNamespace(name) {
      // Not --wait: ssh to the demo machine drops a command that stays silent for about 3 minutes,
      // so the wait for the namespace's finalizers is a poll of short commands instead.
      await box.kubectl(["delete", "namespace", name, "--ignore-not-found", "--wait=false"]);
      const deadline = Date.now() + NAMESPACE_WAIT_MS;
      while ((await box.getJson(["get", "namespace", name])) !== undefined) {
        if (Date.now() > deadline) throw new ReleaseError(`timed out after ${NAMESPACE_WAIT_MS / 1000} s waiting for namespace ${name} to be deleted; see its status.conditions`);
        await Bun.sleep(5_000);
      }
    },
    async volumesLeft(claims) {
      return (await box.items(["get", "persistentvolumes"])).filter((pv) => {
        const ref = (pv.spec as { claimRef?: { namespace?: string; name?: string } }).claimRef;
        return ref !== undefined && claims.has(`${ref.namespace}/${ref.name}`);
      }).length;
    },
    sleep: (ms) => Bun.sleep(ms),
  };
}

let log: Log | undefined;

if (import.meta.main) {
  await main(
    () => log,
    async () => {
      const options = commonOptions(parseOptions(Bun.argv.slice(2), COMMON_SPEC));
      log = new Log("cleanup", options.logDir);
      await runCleanup(boxOps(new Box(options.sshHost, log), options), log, options);
    },
  );
}
