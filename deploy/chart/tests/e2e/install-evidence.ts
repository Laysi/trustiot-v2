// Evidence of a failed release install of the cluster e2e tests (Laysi/trustiot-v2#399): the
// events of the test namespace and the state of its Deployments, ReplicaSets and Pods, as text for
// the nightly report. The repository is public and the report is posted in issues, so the text is
// built from an allow-list of status fields. A Pod's spec (environment values, volumes), Secrets
// and ConfigMaps are never read into it, and a free-text field is cut to one short line. Container
// logs are not collected either: an application can log a credential, and a log line cannot be
// told apart from one that is safe to publish. What the evidence can still hold is text that
// Kubernetes controllers and the kubelet write (conditions, event messages, waiting reasons), and
// two kinds of text in them are removed too: a URL's user info and query string (a registry or
// webhook URL can carry a token), and the output of a failed probe (a command probe's event
// message is what the command printed).
//
// The nightly diagnostics (scripts/nightly/diagnostics.ts, Laysi/trustiot-v2#410) build on the
// same formatting and the same rules.
//
// The formatting is pure (JSON in, text out) so that it is tested without a cluster
// (install-evidence.test.ts). `collectInstallEvidence` only runs kubectl and feeds it.

type Json = Record<string, unknown>;

/** Runs kubectl in the test namespace and returns its standard output; throws when it fails. */
export type Kubectl = (args: string[]) => string;

/** The longest free-text field (a condition or event message) kept in the evidence. */
export const MAX_FIELD_CHARS = 300;
/** The events kept: the newest ones, because the cause of a failed install is the last few. */
export const MAX_EVENTS = 80;

export const isObject = (value: unknown): value is Json => typeof value === "object" && value !== null && !Array.isArray(value);
export const asObject = (value: unknown): Json => (isObject(value) ? value : {});
export const asArray = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);

export function text(value: unknown): string {
  return typeof value === "string" ? value : typeof value === "number" || typeof value === "boolean" ? String(value) : "";
}

/**
 * What a URL in a free-text field may carry besides where it points: `user:password@` before the
 * host and a query string (`?token=...`) after the path.
 */
export function withoutUrlSecrets(value: string): string {
  return value
    .replace(/([a-z][a-z0-9+.-]*:\/\/[^\s?#"']*)\?[^\s"']*/gi, "$1?(query removed)")
    .replace(/([a-z][a-z0-9+.-]*:\/\/)[^\s/"']*@/gi, "$1(user info removed)@");
}

/** The end of a long field that is kept: a chain of errors (an image pull) ends in the real cause. */
export const KEPT_TAIL_CHARS = 100;

/**
 * One line, at most `MAX_FIELD_CHARS` characters of text, without what `withoutUrlSecrets`
 * removes. A longer text keeps its start and its last `KEPT_TAIL_CHARS` characters.
 */
export function oneLine(value: unknown): string {
  const flat = withoutUrlSecrets(text(value)).replace(/\s+/g, " ").trim();
  if (flat.length <= MAX_FIELD_CHARS) return flat;
  const head = MAX_FIELD_CHARS - KEPT_TAIL_CHARS;
  return `${flat.slice(0, head)}... (${flat.length - MAX_FIELD_CHARS} characters cut) ...${flat.slice(-KEPT_TAIL_CHARS)}`;
}

export function field(object: Json, ...path: string[]): unknown {
  let current: unknown = object;
  for (const key of path) {
    if (!isObject(current)) return undefined;
    current = current[key];
  }
  return current;
}

export function nameOf(object: Json): string {
  return oneLine(field(object, "metadata", "name")) || "(no name)";
}

export function itemsOf(list: unknown): Json[] {
  return asArray(asObject(list).items).filter(isObject);
}

/** `Type=status(reason: message)` for each condition, in the order the API reports them. */
export function conditionsOf(status: Json): string {
  return asArray(status.conditions)
    .filter(isObject)
    .map((condition) => {
      const detail = [oneLine(condition.reason), oneLine(condition.message)].filter((part) => part !== "").join(": ");
      return `${oneLine(condition.type)}=${oneLine(condition.status)}${detail === "" ? "" : ` (${detail})`}`;
    })
    .join("; ");
}

export function count(status: Json, key: string): string {
  return text(status[key]) || "0";
}

export function ownerOf(object: Json): string {
  const owner = asArray(field(object, "metadata", "ownerReferences")).filter(isObject)[0];
  return owner === undefined ? "" : ` owner=${oneLine(owner.kind)}/${oneLine(owner.name)}`;
}

export function formatDeployments(list: unknown): string[] {
  return itemsOf(list).map((deployment) => {
    const status = asObject(deployment.status);
    const desired = text(field(deployment, "spec", "replicas")) || "1";
    const generation = text(field(deployment, "metadata", "generation"));
    const observed = text(status.observedGeneration);
    return [
      `Deployment/${nameOf(deployment)}: desired=${desired} replicas=${count(status, "replicas")} updated=${count(status, "updatedReplicas")}`,
      `ready=${count(status, "readyReplicas")} available=${count(status, "availableReplicas")}`,
      `generation=${generation}/observed=${observed}`,
      conditionsOf(status) === "" ? "" : `conditions: ${conditionsOf(status)}`,
    ]
      .filter((part) => part !== "")
      .join(" ");
  });
}

export function formatReplicaSets(list: unknown): string[] {
  return itemsOf(list).map((replicaSet) => {
    const status = asObject(replicaSet.status);
    const desired = text(field(replicaSet, "spec", "replicas")) || "0";
    return [
      `ReplicaSet/${nameOf(replicaSet)}:${ownerOf(replicaSet)} desired=${desired} replicas=${count(status, "replicas")}`,
      `ready=${count(status, "readyReplicas")} available=${count(status, "availableReplicas")}`,
      conditionsOf(status) === "" ? "" : `conditions: ${conditionsOf(status)}`,
    ]
      .filter((part) => part !== "")
      .join(" ");
  });
}

/**
 * The reasons a probe gives for failing that Go's HTTP client and the kubelet write, as the text
 * to show for each. The text shown is always one of these (or the three digits of a status code),
 * never part of the event message: a command probe's message is what the command printed, and a
 * pattern that passed the message through could be matched by a command that prints
 * `context deadline exceeded <value>` or `Get "http://host/<value>": EOF`.
 */
const REQUEST = String.raw`(?:(?:Get|Head) "[^"]*": )?`;
const PROBE_ERRORS: readonly (readonly [RegExp, string])[] = [
  [new RegExp(String.raw`^${REQUEST}dial tcp \S+: connect: connection refused$`), "dial tcp: connection refused"],
  [new RegExp(String.raw`^${REQUEST}dial tcp \S+: connect: no route to host$`), "dial tcp: no route to host"],
  [new RegExp(String.raw`^${REQUEST}dial tcp \S+: connect: network is unreachable$`), "dial tcp: network is unreachable"],
  [new RegExp(String.raw`^${REQUEST}(?:read|write) tcp \S+->\S+: (?:read|write): connection reset by peer$`), "connection reset by peer"],
  [new RegExp(String.raw`^${REQUEST}context deadline exceeded(?: \(Client\.Timeout exceeded while awaiting headers\))?$`), "context deadline exceeded"],
  [new RegExp(String.raw`^${REQUEST}EOF$`), "EOF"],
];

/**
 * A failed probe's event message with the probe's output only when it is a reason the kubelet or
 * the network stack wrote (`HTTP probe failed with statuscode: 503`, `connection refused`), shown in
 * a fixed wording. Anything else, a command probe's output above all, is cut to the part the
 * kubelet wrote: `Readiness probe failed: (probe output not shown)`. The caller passes the message
 * of an `Unhealthy` event, which is always a probe's.
 */
export function probeMessage(message: string): string {
  const match = /^((?:Readiness|Liveness|Startup) probe (?:failed|errored and resulted in unknown state)): ([\s\S]*)$/.exec(message);
  // Not the kubelet's wording, so not known to be free of an application's text.
  if (match === null) return "(probe message not shown)";
  const [, head = "", rawOutput = ""] = match;
  const output = rawOutput.trim();
  const status = /^HTTP probe failed with statuscode: (\d{3})$/.exec(output)?.[1];
  if (status !== undefined) return `${head}: HTTP probe failed with statuscode: ${status}`;
  const known = PROBE_ERRORS.find(([pattern]) => pattern.test(output));
  return known === undefined ? `${head}: (probe output not shown)` : `${head}: ${known[1]}`;
}

/** `name: running`, `name: waiting (ImagePullBackOff: ...)` or `name: terminated (exit 1, Error)`. */
function containerState(status: Json): string {
  const state = asObject(status.state);
  const waiting = asObject(state.waiting);
  const terminated = asObject(state.terminated);
  const lastTerminated = asObject(field(status, "lastState", "terminated"));
  let current = "unknown";
  if (isObject(state.running)) current = "running";
  else if (Object.keys(waiting).length > 0) {
    const detail = [oneLine(waiting.reason), oneLine(waiting.message)].filter((part) => part !== "").join(": ");
    current = `waiting${detail === "" ? "" : ` (${detail})`}`;
  } else if (Object.keys(terminated).length > 0) {
    current = `terminated (exit ${text(terminated.exitCode)}, ${oneLine(terminated.reason) || "no reason"})`;
  }
  const previous =
    Object.keys(lastTerminated).length > 0
      ? `; last exit ${text(lastTerminated.exitCode)}, ${oneLine(lastTerminated.reason) || "no reason"}`
      : "";
  return `${oneLine(status.name)}: ${current}, ready=${text(status.ready)}, restarts=${count(status, "restartCount")}${previous}`;
}

export function formatPods(list: unknown): string[] {
  return itemsOf(list).flatMap((pod) => {
    const status = asObject(pod.status);
    const containers = [...asArray(status.initContainerStatuses), ...asArray(status.containerStatuses)].filter(isObject);
    const head = [
      `Pod/${nameOf(pod)}:${ownerOf(pod)} phase=${oneLine(status.phase) || "unknown"}`,
      oneLine(status.reason) === "" ? "" : `reason=${oneLine(status.reason)}`,
      oneLine(status.message) === "" ? "" : `message=${oneLine(status.message)}`,
      conditionsOf(status) === "" ? "" : `conditions: ${conditionsOf(status)}`,
    ]
      .filter((part) => part !== "")
      .join(" ");
    return [head, ...containers.map((container) => `  ${containerState(container)}`)];
  });
}

/** Events, oldest first, the newest `MAX_EVENTS` of them. */
export function formatEvents(list: unknown): string[] {
  const events = itemsOf(list).map((event) => {
    const when = oneLine(event.lastTimestamp) || oneLine(event.eventTime) || oneLine(field(event, "metadata", "creationTimestamp"));
    const times = Number(text(event.count)) > 1 ? ` x${text(event.count)}` : "";
    return {
      when,
      line: `${when || "(no time)"} ${oneLine(event.type) || "Normal"} ${oneLine(event.reason)} ${oneLine(field(event, "involvedObject", "kind"))}/${oneLine(field(event, "involvedObject", "name"))}${times}: ${oneLine(event.reason === "Unhealthy" ? probeMessage(text(event.message)) : event.message)}`,
    };
  });
  // Timestamps are RFC 3339 in UTC, so their text sorts like the time.
  events.sort((a, b) => a.when.localeCompare(b.when));
  const kept = events.slice(-MAX_EVENTS);
  const cut = events.length - kept.length;
  return [...(cut > 0 ? [`(${cut} older events not shown)`] : []), ...kept.map((event) => event.line)];
}

export interface EvidenceInput {
  /** Result of `kubectl get <kind> --output json`, or the reason it could not be read. */
  events: unknown | Error;
  deployments: unknown | Error;
  replicaSets: unknown | Error;
  pods: unknown | Error;
  /** Output of `helm history`. */
  history?: string | Error;
}

function section(title: string, body: unknown | Error, format: (value: unknown) => string[]): string {
  if (body instanceof Error) return `### ${title}\n(not available: ${oneLine(body.message)})\n`;
  const lines = format(body);
  return `### ${title}\n${lines.length === 0 ? "(none)" : lines.join("\n")}\n`;
}

/** The evidence text: one section per kind, then the release history. */
export function formatInstallEvidence(input: EvidenceInput): string {
  const sections = [
    section("Deployments", input.deployments, formatDeployments),
    section("ReplicaSets", input.replicaSets, formatReplicaSets),
    section("Pods", input.pods, formatPods),
    section("Events", input.events, formatEvents),
  ];
  if (input.history !== undefined) {
    sections.push(section("helm history", input.history, (value) => text(value).trim().split("\n").map(oneLine)));
  }
  return sections.join("\n");
}

function readJson(kubectl: Kubectl, kind: string): unknown | Error {
  try {
    return JSON.parse(kubectl(["get", kind, "--output", "json"]));
  } catch (error) {
    return error instanceof Error ? error : new Error(String(error));
  }
}

/**
 * Reads the namespace and formats the evidence. Never throws: a kubectl command that fails (the
 * API server may be the very thing that is failing) is reported in its section.
 */
export function collectInstallEvidence(kubectl: Kubectl, history: () => string): string {
  let helmHistory: string | Error;
  try {
    helmHistory = history();
  } catch (error) {
    helmHistory = error instanceof Error ? error : new Error(String(error));
  }
  return formatInstallEvidence({
    events: readJson(kubectl, "events"),
    deployments: readJson(kubectl, "deployments"),
    replicaSets: readJson(kubectl, "replicasets"),
    pods: readJson(kubectl, "pods"),
    history: helmHistory,
  });
}
