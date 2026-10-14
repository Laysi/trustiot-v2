// The diagnostics of a failed nightly shape (Laysi/trustiot-v2#410): what is not ready and why,
// as text for the report. The report is posted in the job summary, the artifact and the
// `nightly-failure` issue, and the repository is public, so the text is built the way the evidence
// of a failed install is (deploy/chart/tests/e2e/install-evidence.ts, Laysi/trustiot-v2#399):
//
// - only from an allow-list of status fields (conditions, replica counts, container states, restart
//   counts, the reason and exit code of the last termination, events);
// - no Pod spec, so no environment value; no Secret or ConfigMap contents, only their names;
// - no container log, and no `kubectl describe`, which prints the spec and the annotations.
//
// A Pod's termination message and a log line are written by the application and can hold
// anything, including a credential that cannot be told apart from a line that is safe to publish.
// What is kept is what Kubernetes controllers and the kubelet write about the Pod (`OOMKilled`,
// `exit 137`, `ImagePullBackOff`, `CrashLoopBackOff`, a failed probe's status code).
//
// The formatting is pure (JSON in, text out) so that it is tested without a cluster
// (diagnostics.test.ts); `gatherDiagnostics` only runs the commands and feeds it.
import {
  asArray,
  asObject,
  conditionsOf,
  count,
  field,
  formatDeployments,
  formatEvents,
  formatPods,
  formatReplicaSets,
  isObject,
  itemsOf,
  nameOf,
  oneLine,
  text,
} from "../../deploy/chart/tests/e2e/install-evidence.ts";

type Json = Record<string, unknown>;

/** Runs a command and returns its standard output; throws when it fails. */
export type Command = (args: string[]) => Promise<string>;

/** The longest a section of the diagnostics may be before the report keeps only its end. */
const MAX_SECTION_LINES = 120;

const present = (...parts: string[]): string => parts.filter((part) => part !== "").join(" ");
const withConditions = (status: Json): string => (conditionsOf(status) === "" ? "" : `conditions: ${conditionsOf(status)}`);

/** StatefulSets: the Deployment's counts under the StatefulSet's own names. */
export function formatStatefulSets(list: unknown): string[] {
  return itemsOf(list).map((set) => {
    const status = asObject(set.status);
    const desired = text(field(set, "spec", "replicas")) || "1";
    return present(
      `StatefulSet/${nameOf(set)}: desired=${desired} replicas=${count(status, "replicas")} ready=${count(status, "readyReplicas")}`,
      `updated=${count(status, "updatedReplicas")} available=${count(status, "availableReplicas")}`,
      withConditions(status),
    );
  });
}

export function formatDaemonSets(list: unknown): string[] {
  return itemsOf(list).map((set) => {
    const status = asObject(set.status);
    return present(
      `DaemonSet/${nameOf(set)}: desired=${count(status, "desiredNumberScheduled")} current=${count(status, "currentNumberScheduled")}`,
      `ready=${count(status, "numberReady")} available=${count(status, "numberAvailable")}`,
      withConditions(status),
    );
  });
}

export function formatJobs(list: unknown): string[] {
  return itemsOf(list).map((job) => {
    const status = asObject(job.status);
    return present(
      `Job/${nameOf(job)}: active=${count(status, "active")} succeeded=${count(status, "succeeded")} failed=${count(status, "failed")}`,
      withConditions(status),
    );
  });
}

export function formatVolumeClaims(list: unknown): string[] {
  return itemsOf(list).map((claim) => `PersistentVolumeClaim/${nameOf(claim)}: phase=${oneLine(field(claim, "status", "phase")) || "unknown"}`);
}

/** Node conditions: a node under memory or disk pressure evicts pods, which looks like a crash. */
export function formatNodes(list: unknown): string[] {
  return itemsOf(list).map((node) => present(`Node/${nameOf(node)}:`, withConditions(asObject(node.status))));
}

/**
 * CloudNativePG Clusters: the operator's status fields only. The spec (bootstrap, secrets,
 * certificates) and the status fields that name secrets or certificates are not read.
 */
export function formatPostgresClusters(list: unknown): string[] {
  return itemsOf(list).map((cluster) => {
    const status = asObject(cluster.status);
    const instances = asObject(status.instancesStatus);
    const byState = Object.entries(instances)
      .map(([state, pods]) => `${oneLine(state)}=[${asArray(pods).map((pod) => oneLine(pod)).join(", ")}]`)
      .join(" ");
    return present(
      `Cluster/${nameOf(cluster)}: phase=${oneLine(status.phase) || "unknown"}`,
      oneLine(status.phaseReason) === "" ? "" : `reason=${oneLine(status.phaseReason)}`,
      `instances=${count(status, "instances")} ready=${count(status, "readyInstances")}`,
      oneLine(status.currentPrimary) === "" ? "" : `primary=${oneLine(status.currentPrimary)}`,
      byState === "" ? "" : `instancesStatus: ${byState}`,
      withConditions(status),
    );
  });
}

/** Gateways: the conditions of the Gateway and of each listener. */
export function formatGateways(list: unknown): string[] {
  return itemsOf(list).flatMap((gateway) => {
    const status = asObject(gateway.status);
    const listeners = asArray(status.listeners)
      .filter(isObject)
      .map((listener) => `  listener ${oneLine(listener.name)}: attachedRoutes=${count(listener, "attachedRoutes")} ${withConditions(listener)}`.trimEnd());
    return [present(`Gateway/${nameOf(gateway)}:`, withConditions(status)), ...listeners];
  });
}

/** Pods of every namespace but the smoke test's own, grouped by namespace: the platform's view. */
export function formatPlatformPods(list: unknown, exceptNamespace: string): string[] {
  const byNamespace = new Map<string, Json[]>();
  for (const pod of itemsOf(list)) {
    const namespace = oneLine(field(pod, "metadata", "namespace")) || "(no namespace)";
    if (namespace === exceptNamespace) continue;
    byNamespace.set(namespace, [...(byNamespace.get(namespace) ?? []), pod]);
  }
  return [...byNamespace]
    .sort(([a], [b]) => a.localeCompare(b))
    .flatMap(([namespace, pods]) => [`namespace ${namespace}:`, ...formatPods({ items: pods })]);
}

/** Warning events of the other namespaces, with the namespace in front of the object's name. */
export function formatPlatformWarnings(list: unknown, exceptNamespace: string): string[] {
  const warnings = itemsOf(list)
    .filter((event) => oneLine(event.type) === "Warning")
    .filter((event) => (oneLine(field(event, "metadata", "namespace")) || oneLine(field(event, "involvedObject", "namespace"))) !== exceptNamespace)
    .map((event) => {
      const namespace = oneLine(field(event, "metadata", "namespace")) || oneLine(field(event, "involvedObject", "namespace"));
      const involved = asObject(event.involvedObject);
      return { ...event, involvedObject: { ...involved, name: `${namespace}/${oneLine(involved.name)}` } };
    });
  return formatEvents({ items: warnings });
}

export interface DiagnosticsInput {
  /** Output of `helm list`. */
  helmList: string | Error;
  /** Output of `kubectl get <kinds> --output name`: names only, so a Secret's name but not its data. */
  objectNames: string | Error;
  /** Results of `kubectl get <kind> --output json`, or the reason it could not be read. */
  nodes: unknown | Error;
  deployments: unknown | Error;
  statefulSets: unknown | Error;
  daemonSets: unknown | Error;
  replicaSets: unknown | Error;
  jobs: unknown | Error;
  pods: unknown | Error;
  volumeClaims: unknown | Error;
  postgresClusters: unknown | Error;
  gateways: unknown | Error;
  events: unknown | Error;
  /** Pods and events of all namespaces. */
  allPods: unknown | Error;
  allEvents: unknown | Error;
}

function section(title: string, body: unknown | Error, format: (value: unknown) => string[]): string {
  if (body instanceof Error) return `### ${title}\n(not available: ${oneLine(body.message)})\n`;
  const lines = format(body);
  if (lines.length === 0) return `### ${title}\n(none)\n`;
  const kept = lines.length > MAX_SECTION_LINES ? [`(${lines.length - MAX_SECTION_LINES} earlier lines not shown)`, ...lines.slice(-MAX_SECTION_LINES)] : lines;
  return `### ${title}\n${kept.join("\n")}\n`;
}

/** Output of a tool that prints names or a table: every line cut to one short line. */
const plainLines = (value: unknown): string[] => text(value).trim().split("\n").filter((line) => line.trim() !== "").map(oneLine);

/**
 * The diagnostics text. The sections that explain the most come last, because the report keeps the
 * end of a long text: the namespace's workloads and Pods, then its events.
 */
export function formatDiagnostics(namespace: string, input: DiagnosticsInput): string {
  return [
    section("helm list", input.helmList, plainLines),
    section("object names (no data)", input.objectNames, plainLines),
    section("Nodes", input.nodes, formatNodes),
    section("Pods and warning events of the other namespaces (Istio, CloudNativePG operator, k3s)", input.allPods, (value) => [
      ...formatPlatformPods(value, namespace),
      ...(input.allEvents instanceof Error ? [`(events not available: ${oneLine(input.allEvents.message)})`] : formatPlatformWarnings(input.allEvents, namespace)),
    ]),
    section("CloudNativePG Clusters", input.postgresClusters, formatPostgresClusters),
    section("Gateways", input.gateways, formatGateways),
    section("PersistentVolumeClaims", input.volumeClaims, formatVolumeClaims),
    section("Jobs", input.jobs, formatJobs),
    section("StatefulSets", input.statefulSets, formatStatefulSets),
    section("DaemonSets", input.daemonSets, formatDaemonSets),
    section("Deployments", input.deployments, formatDeployments),
    section("ReplicaSets", input.replicaSets, formatReplicaSets),
    section("Pods", input.pods, formatPods),
    section("Events", input.events, formatEvents),
  ].join("\n");
}

/** `kubectl get` of kinds in the namespace as JSON, or the reason it failed. */
async function readJson(command: Command, args: string[]): Promise<unknown | Error> {
  try {
    return JSON.parse(await command(args));
  } catch (error) {
    return error instanceof Error ? error : new Error(String(error));
  }
}

async function readText(command: Command, args: string[]): Promise<string | Error> {
  try {
    return await command(args);
  } catch (error) {
    return error instanceof Error ? error : new Error(String(error));
  }
}

/** Every Secret and ConfigMap is listed by name only (`--output name`): a missing reference is visible, data is not. */
export const OBJECT_KINDS = "all,configmap,secret,serviceaccount,pvc";

/**
 * Reads the namespace and the platform and formats the diagnostics. The commands it runs are
 * `kubectl get` (JSON, or `--output name` for the object list) and `helm list`: never `logs`,
 * `describe` or `get secret`. It never throws: a command that fails (the API server may be the
 * very thing that is failing) is reported in its section.
 */
export async function gatherDiagnostics(
  namespace: string,
  tools: { kubectl: Command; helmList: () => Promise<string> },
): Promise<string> {
  const inNamespace = ["--namespace", namespace];
  const get = (kind: string, scope: string[] = inNamespace) => readJson(tools.kubectl, ["get", kind, ...scope, "--output", "json"]);
  const [objectNames, helmList, nodes, deployments, statefulSets, daemonSets, replicaSets, jobs, pods, volumeClaims, postgresClusters, gateways, events, allPods, allEvents] =
    await Promise.all([
      readText(tools.kubectl, ["get", OBJECT_KINDS, ...inNamespace, "--output", "name"]),
      tools.helmList().catch((error: unknown) => (error instanceof Error ? error : new Error(String(error)))),
      get("nodes", []),
      get("deployments"),
      get("statefulsets"),
      get("daemonsets"),
      get("replicasets"),
      get("jobs"),
      get("pods"),
      get("persistentvolumeclaims"),
      get("clusters.postgresql.cnpg.io"),
      get("gateways.gateway.networking.k8s.io"),
      get("events"),
      get("pods", ["--all-namespaces"]),
      get("events", ["--all-namespaces"]),
    ]);
  return formatDiagnostics(namespace, {
    helmList, objectNames, nodes, deployments, statefulSets, daemonSets, replicaSets, jobs, pods, volumeClaims, postgresClusters, gateways, events, allPods, allEvents,
  });
}
