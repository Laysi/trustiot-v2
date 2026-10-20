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
 *   nothing else is in it;
 * - ClusterRoleBindings whose subjects are all in those namespaces (the demo reuses `trustiot`, and
 *   a ServiceAccount of the same name there would inherit their rights).
 *
 * The istiod ext_authz provider that points at the PoC is replaced by platform.ts, after this.
 * Nothing on the demo machine's file system is touched. Usage (scripts/README.md, 「展示機」):
 *
 *   bun scripts/demo/cleanup.ts --ssh-host <host>                       # plan only, read only
 *   bun scripts/demo/cleanup.ts --ssh-host <host> --apply --confirm <code>
 *
 * The plan names every object that goes (objects that another object owns, such as Pods and
 * ReplicaSets, go with their owner and are not listed) and the uid of each namespace and binding
 * that goes; every delete carries that uid as a Kubernetes precondition, so an object made again
 * under the same name meanwhile stays. A namespace is only
 * removed while it still looks like the PoC: all of the PoC's workloads are there and nothing in it
 * is managed by Helm or Flux. So once the demo environment runs in `trustiot`, this script leaves it
 * alone. Anything ambiguous (some of the PoC's workloads, the PoC's next to Helm's, a binding with
 * subjects elsewhere too, a webhook or API service that calls into a namespace that would go) stops
 * the run before anything changes. Right before each namespace goes, it is inventoried again, and
 * anything new in it stops the run. See lib.ts for the lock, the confirmation code and the records.
 */

import { type RunResult, parseOptions } from "../release/exec.ts";
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
  lastLines,
  main,
  opsTunnel,
  shownPath,
  stableJson,
  stateDiff,
  subjectRef,
  takeLock,
  withOpsTunnel,
  workloads,
} from "./lib.ts";

/** The issue whose lab lock this script takes. */
export const ISSUE = "Laysi/trustiot-v2#465";
export const EDGE_NAMESPACE = "istio-ingress";
/** The PoC's namespaces, in the order the plan lists them. */
export const POC_NAMESPACES = [DEMO_NAMESPACE, "poc"] as const;
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
  /** Helm or Flux manages it: a Helm label or annotation, a Helm release record, a Flux label or a Flux object. */
  managed: boolean;
}

export interface NamespaceFacts {
  name: string;
  exists: boolean;
  /** The namespace's metadata.uid: a namespace deleted and made again under the same name differs. */
  uid?: string;
  /** The namespace itself carries a Helm or Flux label or annotation. */
  managed: boolean;
  objects: ObjectFact[];
}

export interface Subject {
  kind: string;
  namespace?: string;
  name: string;
}

/** Cluster-scoped objects that point into namespaces (only what the plan needs). */
export interface ClusterRefs {
  bindings: { name: string; uid: string; roleRef: string; subjects: Subject[]; managed: boolean }[];
  /** Webhook configurations, APIServices and CRD conversion webhooks, with the namespaces of the services they call. */
  callers: { kind: string; name: string; namespaces: string[] }[];
}

export interface CleanupPlan {
  /** Namespaces that go, with their uid and every object in them that is not owned by another one. */
  deleteNamespaces: { name: string; uid: string; objects: string[]; volumeClaims: string[] }[];
  /** Objects that go from a namespace that stays. */
  deleteObjects: { namespace: string; uid: string; objects: string[] }[];
  /** ClusterRoleBindings (with their uid) whose subjects are all in namespaces that go or are gone; their ClusterRoles stay. */
  deleteBindings: { name: string; uid: string; roleRef: string; subjects: string[] }[];
  keep: string[];
  /** What needs a person: the run stops before anything changes. */
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
 * istio-ingress goes whole when it holds only the edge PoC's objects and the namespace defaults,
 * none of them managed; otherwise only the edge PoC's own objects go, and refused when Helm or Flux
 * manages one of them.
 * Cluster-scoped references into the namespaces that go (or that are already gone, after an
 * interrupted run): a ClusterRoleBinding whose subjects are all there goes, one with other subjects
 * too or managed by Helm or Flux is refused, and a webhook, APIService or CRD conversion that calls
 * a service there is refused.
 */
export function planCleanup(namespaces: readonly NamespaceFacts[], edge: NamespaceFacts, refs: ClusterRefs = { bindings: [], callers: [] }): CleanupPlan {
  const plan: CleanupPlan = { deleteNamespaces: [], deleteObjects: [], deleteBindings: [], keep: [], refused: [] };
  const gone = new Set<string>();
  for (const ns of namespaces) {
    const fingerprint = fingerprintRefs(ns.name);
    if (!ns.exists) {
      plan.keep.push(`namespace ${ns.name}: already gone`);
      gone.add(ns.name);
      continue;
    }
    const refsHere = new Set(ns.objects.map((o) => o.ref));
    const present = fingerprint.filter((ref) => refsHere.has(ref));
    const missing = fingerprint.filter((ref) => !refsHere.has(ref));
    const managed = [...(ns.managed ? ["the namespace"] : []), ...ns.objects.filter((o) => o.managed).map((o) => o.ref)];
    if (present.length === 0) {
      plan.keep.push(`namespace ${ns.name}: none of the PoC's workloads are in it${managed.length > 0 ? " (Helm or Flux manages it)" : ""}; not the PoC`);
    } else if (managed.length > 0) {
      plan.refused.push(`namespace ${ns.name}: the PoC's ${present.join(", ")} next to objects Helm or Flux manages (${managed.slice(0, 5).join(", ")}${managed.length > 5 ? ", ..." : ""})`);
    } else if (missing.length > 0) {
      plan.refused.push(`namespace ${ns.name}: only part of the PoC is there (missing ${missing.join(", ")})`);
    } else {
      plan.deleteNamespaces.push({ name: ns.name, uid: ns.uid ?? "", objects: topLevel(ns), volumeClaims: claimsOf(ns) });
    }
  }

  if (!edge.exists) {
    plan.keep.push(`namespace ${EDGE_NAMESPACE}: already gone`);
    gone.add(EDGE_NAMESPACE);
  } else {
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
      plan.deleteNamespaces.push({ name: EDGE_NAMESPACE, uid: edge.uid ?? "", objects: edgeRefs, volumeClaims: [] });
    } else {
      plan.deleteObjects.push({ namespace: EDGE_NAMESPACE, uid: edge.uid ?? "", objects: ours });
      plan.keep.push(`namespace ${EDGE_NAMESPACE}: also holds ${others.join(", ") || "Helm- or Flux-managed objects"}; only the edge PoC's objects go`);
    }
  }

  const doomed = new Set([...plan.deleteNamespaces.map((ns) => ns.name), ...gone]);
  for (const binding of refs.bindings) {
    const inside = binding.subjects.filter((s) => s.namespace !== undefined && doomed.has(s.namespace));
    if (inside.length === 0) continue;
    const subjects = binding.subjects.map(subjectRef).sort();
    if (binding.managed) {
      plan.refused.push(`ClusterRoleBinding ${binding.name}: Helm or Flux manages it, and its subjects ${subjects.join(", ")} are in namespaces that go`);
    } else if (inside.length !== binding.subjects.length) {
      plan.refused.push(`ClusterRoleBinding ${binding.name}: subjects both in namespaces that go and elsewhere (${subjects.join(", ")}); decide by hand`);
    } else {
      plan.deleteBindings.push({ name: binding.name, uid: binding.uid, roleRef: binding.roleRef, subjects });
      plan.keep.push(`${binding.roleRef} stays (only its binding ${binding.name} goes)`);
    }
  }
  plan.deleteBindings.sort((a, b) => a.name.localeCompare(b.name));
  for (const caller of refs.callers) {
    const into = caller.namespaces.filter((ns) => doomed.has(ns));
    if (into.length > 0) plan.refused.push(`${caller.kind} ${caller.name} calls a service in namespace ${[...new Set(into)].join(", ")}, which goes; decide by hand`);
  }
  return plan;
}

function topLevel(ns: NamespaceFacts): string[] {
  return ns.objects
    .filter((o) => !o.owned)
    .map((o) => o.ref)
    .sort();
}

function claimsOf(ns: NamespaceFacts): string[] {
  return topLevel(ns)
    .filter((ref) => ref.startsWith("persistentvolumeclaim/"))
    .map((ref) => ref.slice("persistentvolumeclaim/".length));
}

/** True when the plan changes nothing. */
export function planIsEmpty(plan: CleanupPlan): boolean {
  return plan.deleteNamespaces.length === 0 && plan.deleteObjects.length === 0 && plan.deleteBindings.length === 0;
}

/**
 * Why a namespace, inventoried again right before it goes, no longer matches its plan item
 * (undefined when it still does): another uid, something managed, or an object that was not in
 * the plan. Objects that went meanwhile (the Gateway deleted first) are fine.
 */
export function namespaceDrift(item: { name: string; uid: string; objects: string[] }, now: NamespaceFacts): string | undefined {
  if (!now.exists) return undefined;
  if (now.uid !== item.uid) return `namespace ${item.name} has uid ${now.uid ?? "?"}, the plan has ${item.uid}`;
  if (now.managed || now.objects.some((o) => o.managed)) return `namespace ${item.name} now holds objects Helm or Flux manages`;
  const extra = topLevel(now).filter((ref) => !item.objects.includes(ref));
  return extra.length > 0 ? `namespace ${item.name} now also holds ${extra.join(", ")}` : undefined;
}

// --- Inventory ---------------------------------------------------------------------------------

/**
 * One line per object: apiVersion, kind, name, owner kinds, Secret type, Helm label, Helm release
 * annotation, Flux labels. jsonpath, never `-o json`: the inventory reads Secrets, and their data
 * must not leave the box.
 */
export const INVENTORY_JSONPATH = [
  '{range .items[*]}',
  '{.apiVersion}{"\\t"}{.kind}{"\\t"}{.metadata.name}{"\\t"}{.metadata.ownerReferences[*].kind}{"\\t"}{.type}{"\\t"}',
  '{.metadata.labels.app\\.kubernetes\\.io/managed-by}{"\\t"}',
  '{.metadata.annotations.meta\\.helm\\.sh/release-name}{"\\t"}',
  '{.metadata.labels.helm\\.toolkit\\.fluxcd\\.io/name}{.metadata.labels.kustomize\\.toolkit\\.fluxcd\\.io/name}',
  '{"\\n"}{end}',
].join("");
const INVENTORY_FIELDS = 8;

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
    if (fields.length !== INVENTORY_FIELDS) throw new ReleaseError(`unexpected inventory line (${fields.length} fields): ${line.slice(0, 120)}`);
    const [apiVersion, kind, name, owners, type, helmLabel, helmRelease, fluxLabel] = fields as [string, string, string, string, string, string, string, string];
    if (kind === "" || name === "") throw new ReleaseError(`inventory line without kind or name: ${line.slice(0, 120)}`);
    const group = apiVersion.includes("/") ? apiVersion.slice(0, apiVersion.indexOf("/")) : "";
    objects.push({
      ref: `${kind.toLowerCase()}${group === "" ? "" : `.${group}`}/${name}`,
      // A legacy service account token goes with its ServiceAccount, like an owned object.
      owned: owners.trim() !== "" || type === "kubernetes.io/service-account-token",
      // Every kind of Flux (sources, kustomizations, helm releases, notifications, image
      // automation) lives in a *.toolkit.fluxcd.io group.
      managed: helmLabel === "Helm" || helmRelease !== "" || fluxLabel !== "" || type.startsWith("helm.sh/release") || group.endsWith(".toolkit.fluxcd.io"),
    });
  }
  return objects.sort((a, b) => a.ref.localeCompare(b.ref));
}

function metadataManaged(metadata: { labels?: Record<string, string>; annotations?: Record<string, string> }): boolean {
  const labels = metadata.labels ?? {};
  return (
    labels["app.kubernetes.io/managed-by"] === "Helm" ||
    (metadata.annotations ?? {})["meta.helm.sh/release-name"] !== undefined ||
    Object.keys(labels).some((key) => key.endsWith(".toolkit.fluxcd.io/name"))
  );
}

// --- The run -----------------------------------------------------------------------------------

/** What the run does to the cluster; boxOps does it over ssh, the tests with a fake cluster. */
export interface CleanupOps {
  tunnel(): Promise<OpsTunnel>;
  inventory(namespace: string): Promise<NamespaceFacts>;
  clusterRefs(): Promise<ClusterRefs>;
  lock(): Promise<() => Promise<void>>;
  snapshot(): Promise<{ cluster: Record<string, KindState>; workloads: string[] }>;
  deleteObject(namespace: string, ref: string): Promise<void>;
  /** Deletes a ClusterRoleBinding only while it has this uid (a Kubernetes delete precondition). */
  deleteBinding(name: string, uid: string): Promise<void>;
  /** Deletes a namespace only while it has this uid (a Kubernetes delete precondition), and waits until it is gone. */
  deleteNamespace(name: string, uid: string): Promise<void>;
  /** Persistent volumes still bound to one of these `<namespace>/<claim>`. */
  volumesLeft(claims: ReadonlySet<string>): Promise<number>;
  now(): number;
  sleep(ms: number): Promise<void>;
}

const VOLUME_WAIT_MS = 300_000;
const NAMESPACE_WAIT_MS = 600_000;

async function currentPlan(ops: CleanupOps): Promise<CleanupPlan> {
  const namespaces = [];
  for (const name of POC_NAMESPACES) namespaces.push(await ops.inventory(name));
  return planCleanup(namespaces, await ops.inventory(EDGE_NAMESPACE), await ops.clusterRefs());
}

/**
 * The whole run. Without --apply it only reads. With --apply it checks the confirmation code
 * before it takes the lock or changes anything, re-plans under the lock and stops when the plan
 * changed, inventories each namespace again right before it goes, deletes, waits for the volumes,
 * and checks that nothing outside the plan changed. Returns the plan it worked from.
 */
export async function runCleanup(ops: CleanupOps, log: Log, options: CommonOptions): Promise<CleanupPlan> {
  return withOpsTunnel(
    () => ops.tunnel(),
    log,
    async () => {
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
        await applyPlan(ops, log, plan);
      } finally {
        await release();
      }
      log.note(`done; records in ${shownPath(log.dir)}`);
      return plan;
    },
  );
}

async function applyPlan(ops: CleanupOps, log: Log, plan: CleanupPlan): Promise<void> {
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
  // Bindings before their namespaces: an interrupted run never leaves rights for a namespace name
  // that the demo reuses.
  for (const binding of plan.deleteBindings) {
    // Read it again right before it goes: a subject added elsewhere, another role or a Helm or Flux
    // label since the plan stops the run.
    const now = (await ops.clusterRefs()).bindings.find((b) => b.name === binding.name);
    if (now === undefined) {
      log.note(`ClusterRoleBinding ${binding.name} is already gone`);
      continue;
    }
    if (now.uid !== binding.uid || now.managed || now.roleRef !== binding.roleRef || stableJson(now.subjects.map(subjectRef).sort()) !== stableJson(binding.subjects)) {
      throw new ReleaseError(
        `ClusterRoleBinding ${binding.name} changed since the plan was made (now uid ${now.uid}, ${now.roleRef}, subjects ${now.subjects.map(subjectRef).join(", ")}${now.managed ? ", managed by Helm or Flux" : ""}); stopped before deleting it`,
      );
    }
    // The uid goes with the delete as a precondition: one made again since this read stays.
    await ops.deleteBinding(binding.name, binding.uid);
    log.note(`deleted ClusterRoleBinding ${binding.name} (${binding.subjects.join(", ")}); ${binding.roleRef} stays`);
  }
  for (const item of plan.deleteObjects) {
    // The namespace stays and holds other things by design: it must still be the same namespace,
    // and none of the objects that go may have come under Helm or Flux.
    const now = await ops.inventory(item.namespace);
    const nowManaged = now.objects.filter((o) => o.managed && item.objects.includes(o.ref)).map((o) => o.ref);
    if (now.exists && now.uid !== item.uid) throw new ReleaseError(`namespace ${item.namespace} has uid ${now.uid ?? "?"}, the plan has ${item.uid}; stopped before deleting anything more in it`);
    if (now.managed) throw new ReleaseError(`Helm or Flux now manages namespace ${item.namespace}; stopped before deleting anything more in it`);
    if (nowManaged.length > 0) throw new ReleaseError(`Helm or Flux now manages ${nowManaged.join(", ")} in ${item.namespace}; stopped before deleting anything more in it`);
    for (const ref of item.objects.filter((o) => o !== EDGE_GATEWAY)) {
      await ops.deleteObject(item.namespace, ref);
      log.note(`deleted ${ref} in ${item.namespace}`);
    }
  }
  // istio-ingress first, then the namespaces whose services it used.
  for (const ns of [...plan.deleteNamespaces].sort((a, b) => Number(b.name === EDGE_NAMESPACE) - Number(a.name === EDGE_NAMESPACE))) {
    // The deletes before this one can take minutes: look again right before this one goes.
    const drift = namespaceDrift(ns, await ops.inventory(ns.name));
    if (drift !== undefined) throw new ReleaseError(`${drift}; stopped before deleting it`);
    // The planned bindings are gone by now, so any binding with a subject here, or anything that
    // calls into it, came after the plan: deleting the namespace would leave rights for a
    // ServiceAccount of the same name the demo makes later.
    const refs = await ops.clusterRefs();
    const into = [
      ...refs.bindings.filter((b) => b.subjects.some((s) => s.namespace === ns.name)).map((b) => `ClusterRoleBinding ${b.name}`),
      ...refs.callers.filter((c) => c.namespaces.includes(ns.name)).map((c) => `${c.kind} ${c.name}`),
    ];
    if (into.length > 0) throw new ReleaseError(`${into.join(", ")} now point${into.length === 1 ? "s" : ""} into namespace ${ns.name}, not in the plan; stopped before deleting it`);
    log.note(`deleting namespace ${ns.name} (${ns.objects.length} objects; volume claims: ${ns.volumeClaims.join(", ") || "none"})`);
    // The planned uid goes with the delete as a precondition: a namespace made again under the
    // same name between the look above and the delete stays.
    await ops.deleteNamespace(ns.name, ns.uid);
    log.note(`deleted namespace ${ns.name}`);
  }
  // local-path removes a volume and its directory with a helper pod after the claim is gone.
  const claims = new Set(plan.deleteNamespaces.flatMap((ns) => ns.volumeClaims.map((claim) => `${ns.name}/${claim}`)));
  if (claims.size > 0) {
    const deadline = ops.now() + VOLUME_WAIT_MS;
    for (;;) {
      const left = await ops.volumesLeft(claims);
      if (left === 0) break;
      if (ops.now() > deadline) {
        throw new ReleaseError(`timed out after ${VOLUME_WAIT_MS / 1000} s waiting for the persistent volumes of the removed claims (${left} left); check the local-path provisioner in kube-system`);
      }
      await ops.sleep(5_000);
    }
    log.note(`the persistent volumes of the ${claims.size} removed claims are deleted`);
  }
  const after = await ops.snapshot();
  log.save("state-after", after);
  log.note(`cluster-scoped changes: ${JSON.stringify(stateDiff(before.cluster, after.cluster))}`);
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
}

/** The clock boxOps waits with; tests replace it. */
export interface Clock {
  now(): number;
  sleep(ms: number): Promise<void>;
}

const UID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** The body of a DELETE that the API server carries out only while the object has this uid. */
export function deleteOptions(uid: string): string {
  if (!UID.test(uid)) throw new ReleaseError(`not a Kubernetes uid: ${JSON.stringify(uid)}; nothing deleted`);
  return JSON.stringify({ kind: "DeleteOptions", apiVersion: "v1", preconditions: { uid }, propagationPolicy: "Background" });
}

/** An object name as one URL path segment. */
function pathSegment(name: string): string {
  if (name === "" || name === "." || name === "..") throw new ReleaseError(`not an object name: ${JSON.stringify(name)}`);
  return encodeURIComponent(name);
}

/**
 * DELETE `path` with a uid precondition (kubectl delete has no flag for one, so the DeleteOptions
 * go as the raw request's body on stdin). The API server answers 409 Conflict, deleting nothing,
 * when the object there has another uid.
 *
 * `kubectl delete --raw <uri>` is a documented kubectl command ("Raw URI to DELETE to the server",
 * https://kubernetes.io/docs/reference/kubectl/generated/kubectl_delete/), and with one `-f` file
 * (`-` is stdin) RunDelete passes that file to rawhttp.RawDelete as the request body
 * (https://github.com/kubernetes/kubernetes/blob/master/staging/src/k8s.io/kubectl/pkg/cmd/delete/delete.go).
 * Checked on 2026-10-20 with the kubectl pinned in .prototools (1.36.5), the same version k3s
 * ships on the demo machine: it sent `DELETE <uri>` with this body to a stand-in endpoint.
 */
function deleteWithUid(box: Box, path: string, uid: string): Promise<RunResult> {
  return box.tryKubectl(["delete", "--raw", path, "-f", "-"], deleteOptions(uid));
}

const SYSTEM_CLOCK: Clock = { now: () => Date.now(), sleep: (ms) => Bun.sleep(ms) };

/** CleanupOps on the demo machine, over ssh. */
export function boxOps(box: Box, options: CommonOptions, clock: Clock = SYSTEM_CLOCK): CleanupOps {
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
      const metadata = ns.metadata as { uid?: string; labels?: Record<string, string>; annotations?: Record<string, string> };
      return { name, exists: true, ...(metadata.uid === undefined ? {} : { uid: metadata.uid }), managed: metadataManaged(metadata), objects: parseInventory(text) };
    },
    async clusterRefs() {
      const bindings = (await box.items(["get", "clusterrolebindings.rbac.authorization.k8s.io"])).map((item) => {
        const metadata = item.metadata as { name: string; uid?: string; labels?: Record<string, string>; annotations?: Record<string, string> };
        const roleRef = item.roleRef as { kind?: string; name?: string } | undefined;
        return {
          name: metadata.name,
          uid: metadata.uid ?? "",
          roleRef: `${roleRef?.kind ?? "ClusterRole"} ${roleRef?.name ?? "?"}`,
          subjects: ((item.subjects as { kind?: string; namespace?: string; name?: string }[] | undefined) ?? []).map((s) => ({
            kind: s.kind ?? "?",
            ...(s.namespace ? { namespace: s.namespace } : {}),
            name: s.name ?? "?",
          })),
          managed: metadataManaged(metadata),
        };
      });
      const callers: ClusterRefs["callers"] = [];
      for (const [resource, kind] of [
        ["validatingwebhookconfigurations.admissionregistration.k8s.io", "ValidatingWebhookConfiguration"],
        ["mutatingwebhookconfigurations.admissionregistration.k8s.io", "MutatingWebhookConfiguration"],
      ] as const) {
        for (const item of await box.items(["get", resource])) {
          const webhooks = (item.webhooks as { clientConfig?: { service?: { namespace?: string } } }[] | undefined) ?? [];
          callers.push({ kind, name: String((item.metadata as { name: string }).name), namespaces: webhooks.flatMap((w) => (w.clientConfig?.service?.namespace ? [w.clientConfig.service.namespace] : [])) });
        }
      }
      for (const item of await box.items(["get", "apiservices.apiregistration.k8s.io"])) {
        const namespace = (item.spec as { service?: { namespace?: string } | null }).service?.namespace;
        callers.push({ kind: "APIService", name: String((item.metadata as { name: string }).name), namespaces: namespace ? [namespace] : [] });
      }
      // jsonpath: the CRDs' full schemas are large and not needed here.
      const crds = await box.kubectl([
        "get",
        "customresourcedefinitions.apiextensions.k8s.io",
        "-o",
        'jsonpath={range .items[*]}{.metadata.name}{"\\t"}{.spec.conversion.webhook.clientConfig.service.namespace}{"\\n"}{end}',
      ]);
      for (const line of crds.split("\n")) {
        const [name, namespace] = line.split("\t");
        if (name) callers.push({ kind: "CustomResourceDefinition (conversion webhook)", name, namespaces: namespace ? [namespace] : [] });
      }
      return { bindings, callers: callers.filter((c) => c.namespaces.length > 0) };
    },
    async lock() {
      const release = await takeLock(box, ISSUE, "cleanup", options.lockHeld);
      // Kinds may have come or gone since the plan was shown: the in-lock re-plan reads them again.
      resources = undefined;
      return release;
    },
    snapshot: async () => ({ cluster: await clusterState(box), workloads: await workloads(box) }),
    async deleteObject(namespace, ref) {
      await box.kubectl(["-n", namespace, "delete", ref, "--ignore-not-found", "--wait", "--timeout=180s"]);
    },
    async deleteBinding(name, uid) {
      const result = await deleteWithUid(box, `/apis/rbac.authorization.k8s.io/v1/clusterrolebindings/${pathSegment(name)}`, uid);
      if (result.exitCode === 0) return;
      // Gone is fine: a delete repeated after ssh dropped the first one finds nothing.
      const now = await box.getJson(["get", "clusterrolebindings.rbac.authorization.k8s.io", name]);
      if (now === undefined) return;
      const nowUid = (now.metadata as { uid?: string } | undefined)?.uid ?? "?";
      throw new ReleaseError(
        `deleting ClusterRoleBinding ${name} with uid ${uid} failed (exit ${result.exitCode}): ${lastLines(result.stderr || result.stdout)}; it is still there${nowUid === uid ? "" : ` with uid ${nowUid}, made again since the plan, and stays`}`,
      );
    },
    async deleteNamespace(name, uid) {
      if (!/^[a-z0-9]([-a-z0-9]*[a-z0-9])?$/.test(name)) throw new ReleaseError(`not a namespace name: ${JSON.stringify(name)}`);
      // Not --wait: ssh to the demo machine drops a command that stays silent for about 3 minutes,
      // so the wait for the namespace's finalizers is a poll of short commands instead.
      const result = await deleteWithUid(box, `/api/v1/namespaces/${name}`, uid);
      const deadline = clock.now() + NAMESPACE_WAIT_MS;
      for (;;) {
        const now = await box.getJson(["get", "namespace", name]);
        if (now === undefined) return;
        const metadata = (now.metadata ?? {}) as { uid?: string; deletionTimestamp?: string };
        if (metadata.uid !== uid) {
          throw new ReleaseError(
            `namespace ${name} now has uid ${metadata.uid ?? "?"}, the plan has ${uid}: it was made again, and the new one stays${result.exitCode === 0 ? "" : ` (the delete: ${lastLines(result.stderr || result.stdout)})`}`,
          );
        }
        // A failed delete is fine only when this namespace is already going: a delete repeated
        // after ssh dropped the first one gets a Conflict while the finalizers run.
        if (result.exitCode !== 0 && metadata.deletionTimestamp === undefined) {
          throw new ReleaseError(`deleting namespace ${name} with uid ${uid} failed (exit ${result.exitCode}): ${lastLines(result.stderr || result.stdout)}`);
        }
        if (clock.now() > deadline) throw new ReleaseError(`timed out after ${NAMESPACE_WAIT_MS / 1000} s waiting for namespace ${name} to be deleted; see its status.conditions`);
        await clock.sleep(5_000);
      }
    },
    async volumesLeft(claims) {
      return (await box.items(["get", "persistentvolumes"])).filter((pv) => {
        const ref = (pv.spec as { claimRef?: { namespace?: string; name?: string } }).claimRef;
        return ref !== undefined && claims.has(`${ref.namespace}/${ref.name}`);
      }).length;
    },
    now: () => clock.now(),
    sleep: (ms) => clock.sleep(ms),
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
