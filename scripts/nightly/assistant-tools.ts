// The assistant's tools in the cluster, as the user, through the mesh (Laysi/trustiot-v2#648): the
// smoke's last step on the release. Everything is the release's own (the assistant, identity,
// assets and telemetry-query services, their mesh policies and the assistant's mesh.calls), but
// the model: helm upgrade sets one more values file that changes only services.ai-gateway.image to
// the scripted stand-in (deploy/chart/tests/e2e/ai-gateway-double), which keeps the workload's
// name, so the AuthorizationPolicy and NetworkPolicy that admit the assistant are the production
// ones. The rest of the smoke ran with the real gateway, whose readiness it covers.
//
// The data, made for the run: two tenants A and B through the identity API (the platform
// administrator's cookie), one editor of each (SQL, as the API check makes its members), one
// device model each through the assets API, and three readings of one random device each, written
// into trustiot.telemetry as the bundled ClickHouse's administrator inside its pod. Every name and
// value carries a canary random to the run (newTenantData), so a tenant's data is recognized
// wherever it shows up. The controls: each member reads its own readings and models through the
// ingress, and A's member reads none of B's.
//
// The checks, through the ingress with a member's cookie; the question carries a directive for the
// stand-in (DIRECTIVE_PREFIX), which calls the tools it names with the inputs it names and then
// answers with what the tools returned. Each call has one expected outcome (PlannedCall), read
// from the callee's code, so no call passes for another reason than the one planned:
//
// 1. A asks for device_models and every telemetry tool on A's device: the run is answered, every
//    call is ok and its result has A's data and none of B's, and the answer has A's model and
//    reading and none of B's canaries.
//    The control: B asks the directive of check 2 on B's own device: every telemetry tool's result
//    has B's data and the answer has B's reading, so B's rows are there and readable as B through
//    the same tools, which makes A's empty results in check 2 mean something.
// 2. A asks the same directive on B's device: every telemetry tool, telemetry_sql with no device
//    filter and telemetry_sql on the base table. telemetry-query answers a device of another
//    tenant with 200 and no data, never 403 or 404, so each telemetry tool must be ok and empty (a
//    denied call would mean the token did not act as A); the SQL without a filter must be ok with
//    A's own text reading only; the base table must be refused (input_error). No B canary may be
//    in the answer or in any result the model got (the stand-in's lines, independently of the
//    answer).
// 3. The logs: the assistant started the three runs for the asker's tenant and completed their
//    tool calls, and the identity service issued each run's delegation and exchanged it for the
//    services the tools called (debug lines, IDENTITY_SMOKE_ENV in api.ts). Events and IDs only;
//    the callees log no user on a call they answer, so the logs tie the calls to the asker's
//    tenant and run, not to the user.
//
// The release's assistant must register every tool (registeredTools): a tool it does not offer to
// the model fails the check. The nightly never runs `kubectl logs` (diagnostics.test.ts), so the
// logs come from where the platform keeps them: the services' events from the log table the
// OpenTelemetry collector writes in the bundled ClickHouse (trustiot_logs.otel_logs, read as its
// administrator inside its pod, selected by event and run ID), and the stand-in's lines from its
// lines port, bound to the pod's loopback, through `kubectl port-forward` to each of its pods
// (GET /nightly/v1/lines, by directive tag). None of it reaches the run log: the failure messages
// name events, IDs, outcomes, counts and the shape of a result (its keys, statuses and row
// counts), never its text, and no message has a cookie or a token. The smoke fails a shape that
// runs the assistant unless this check ran to its last step (toolsCheckProblems).
import { ConnectionError } from "../../deploy/chart/tests/e2e/raw-client.ts";
import type { RawRequest, RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import {
  ACCOUNT_NOT_READY,
  adminSessionSql,
  type ApiCheckContext,
  ASSISTANT_CONVERSATIONS_PATH,
  assistantConversationPath,
  errorCode,
  errorMessageKey,
  ingressAccess,
  memberSessionSql,
  newSession,
  PortForward,
  SESSION_COOKIE,
  telemetryLatestPath,
  TENANTS_PATH,
  type TestSession,
} from "./api.ts";
import { pollUntil } from "./certs.ts";
import { run } from "./exec.ts";
import { clickhousePodOf, type K8sObject } from "./manifests.ts";
import { AI_GATEWAY_DOUBLE_IMAGE } from "./platform.ts";
import type { Check } from "./report.ts";

/** The question prefix the stand-in reads (ai-gateway-double/server.ts, DIRECTIVE). */
export const DIRECTIVE_PREFIX = "nightly-tools ";
/** The assets service's device models (services/assets/src/device_models/mod.rs). */
export const DEVICE_MODELS_PATH = "/api/assets/v1/device-models";
/** Asking a question (services/assistant/src/api/questions.rs, QUESTIONS_PATH). */
export const QUESTIONS_PATH = "/api/assistant/v1/conversations/{conversation_id}/questions";
/** The workload the stand-in replaces: its key under `services` and its container's name. */
export const AI_GATEWAY = "ai-gateway";
/** The data points of the readings. */
export const NUMBER_POINT = "nightly_number";
export const TEXT_POINT = "nightly_text";
/** The gateway and stream the readings come from (the column rules of 0012_gateway_stream_ids.sql). */
export const GATEWAY_ID = "nightly-smoke";
/** The tool that reads the assets service, and the ones that read telemetry-query. */
export const MODEL_TOOL = "device_models";
export const TELEMETRY_TOOLS = ["telemetry_latest", "telemetry_statistics", "telemetry_usage", "telemetry_sql"] as const;
/** Every tool the check calls; the release's assistant must register them all. */
export const ALL_TOOLS: readonly string[] = [MODEL_TOOL, ...TELEMETRY_TOOLS];
/**
 * The callee URLs the chart gives the assistant from mesh.calls.assistant (templates/_assistant.tpl)
 * and the tools each makes it register (services/assistant/src/lib.rs: the four telemetry tools
 * together with telemetry_query.url, device_models with assets.url).
 */
export const TOOLS_OF_URL: Readonly<Record<string, readonly string[]>> = {
  TRUSTIOT__ASSETS__URL: [MODEL_TOOL],
  TRUSTIOT__TELEMETRY_QUERY__URL: TELEMETRY_TOOLS,
};
/** The service each tool calls as the user: the audience of the tokens the identity service exchanges. */
export const AUDIENCE_OF: Readonly<Record<string, string>> = {
  [MODEL_TOOL]: "assets",
  ...Object.fromEntries(TELEMETRY_TOOLS.map((tool) => [tool, "telemetry-query"])),
};
/** The service that holds the run delegations (services/identity/src/delegation.rs). */
export const HOLDER = "assistant";
/** The log events the checks read (services/assistant/src/delegation.rs, tools.rs; services/identity/src/delegation.rs). */
export const RUN_STARTED = "question run started";
export const TOOL_CALL_COMPLETED = "tool call completed";
export const DELEGATION_ISSUED = "issued a run delegation";
export const DELEGATION_EXCHANGED = "exchanged a run delegation";

/** How long the stand-in may take to replace the gateway: the old pod's grace period (315 s) and a margin. */
export const ROLLOUT_LIMIT_MS = 420_000;
/** How long a question may run: two model calls and the tools, on a loaded runner. */
export const ANSWER_LIMIT_MS = 300_000;
/** How long the events of an answered run may take to show in the logs. */
export const LOG_LIMIT_MS = 120_000;
/** How long a new tenant may take to be ready in assets and telemetry-query (api.ts, EVENT_LIMIT_MS). */
export const TENANT_READY_LIMIT_MS = 180_000;
const POLL_MS = 2_000;

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const TAG = /^[a-z0-9]{1,32}$/;

type Json = Record<string, unknown>;
const isMap = (value: unknown): value is Json => typeof value === "object" && value !== null && !Array.isArray(value);
const asMap = (value: unknown): Json => (isMap(value) ? value : {});
const asArray = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);
const asString = (value: unknown): string | undefined => (typeof value === "string" ? value : undefined);

function uuid(value: string): string {
  if (!UUID.test(value)) throw new Error(`not a UUID: ${JSON.stringify(value)}`);
  return value;
}

// ---------------------------------------------------------------------------------------------
// The stand-in in place of the gateway.

/**
 * The values file the last step adds: the stand-in's image for the ai-gateway workload and nothing
 * else. Never pulled: the platform step imported it into every node (platform.ts). The tag is
 * `nightly`, as the chart requires of a service that lists postgres in mesh.calls (migrate.image.tag).
 */
export function aiGatewayDoubleValues(image = AI_GATEWAY_DOUBLE_IMAGE): Record<string, unknown> {
  const at = image.lastIndexOf(":");
  return { services: { [AI_GATEWAY]: { image: { repository: image.slice(0, at), tag: image.slice(at + 1), digest: "", pullPolicy: "Never" } } } };
}

/**
 * The manifest of the release that `helm upgrade --dry-run=server --output json` printed: the
 * objects the upgrade would deploy, rendered and validated by the API server, before anything
 * changes. The output holds Secrets, so it never reaches the log and neither does any of it here.
 */
export function dryRunManifest(stdout: string): string {
  let parsed: unknown;
  try {
    parsed = JSON.parse(stdout);
  } catch {
    throw new Error(`helm's dry run did not print JSON (${stdout.length} characters)`);
  }
  const manifest = asMap(parsed).manifest;
  if (typeof manifest !== "string" || manifest.trim() === "") {
    throw new Error(`helm's dry run printed no manifest (keys ${Object.keys(asMap(parsed)).sort().join(",") || "none"})`);
  }
  return manifest;
}

const keyOf = (o: K8sObject) => `${o.kind}/${o.metadata?.namespace ?? ""}/${o.metadata?.name ?? ""}`;
const nameOf = (o: K8sObject) => `${o.kind}/${o.metadata?.name ?? ""}`;
const isAiGateway = (o: K8sObject) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === AI_GATEWAY;

/** The ai-gateway container of a Deployment, or undefined. */
function aiGatewayContainer(deployment: K8sObject): Json | undefined {
  const containers = asArray(asMap(asMap(asMap(deployment.spec).template).spec).containers).filter(isMap);
  return containers.find((c) => c.name === AI_GATEWAY);
}

const isMigrateJob = (o: K8sObject) => o.kind === "Job" && o.metadata?.labels?.["app.kubernetes.io/component"] === "migrate";
const WORKLOAD_KINDS = new Set(["Deployment", "StatefulSet", "DaemonSet"]);
/** The migration Job's annotation that lists the image of every service that uses Postgres (templates/_migrate.tpl). */
export const SERVICE_IMAGES_ANNOTATION = "trustiot.io/service-images";

function podAnnotations(o: K8sObject): Json {
  const template = asMap(asMap(o.spec).template);
  if (!isMap(template.metadata)) template.metadata = {};
  const metadata = asMap(template.metadata);
  if (!isMap(metadata.annotations)) metadata.annotations = {};
  return asMap(metadata.annotations);
}

/** `name=image,...` as a map. */
function serviceImages(value: unknown): Map<string, string> {
  return new Map(
    String(value ?? "")
      .split(",")
      .filter((entry) => entry !== "")
      .map((entry) => [entry.slice(0, entry.indexOf("=")), entry.slice(entry.indexOf("=") + 1)]),
  );
}

/** What the stand-in's values changed in the release (releaseChange). */
export interface ReleaseChange {
  /** Every difference besides the expected ones; empty when there is none. */
  problems: string[];
  /** The migration Job's name before and after, when the upgrade replaced the Job. */
  migrateJob?: { before: string; after: string };
  /** The workloads besides the gateway whose only change is the migration Job their pods wait for. */
  waiting: string[];
}

/**
 * What differs between the release's objects before and after the stand-in's values. Expected:
 * the ai-gateway container's image (now `image`) and pull policy (Never), and what the chart
 * derives from any new image of a service that uses Postgres, as the AI gateway does: the
 * migration Job's `trustiot.io/service-images` names the new image, so the Job's name (the hash
 * of its spec) changes, the Role that lets the services read it names the new Job, and every
 * service's wait-for-migrations init container waits for it, so their pods are replaced with the
 * same images and settings (templates/_migrate.tpl). Anything else is a problem. Used on
 * `helm template` (assistant-tools.test.ts) and on the deployed release.
 */
export function releaseChange(before: K8sObject[], after: K8sObject[], image = AI_GATEWAY_DOUBLE_IMAGE): ReleaseChange {
  const problems: string[] = [];
  const oldJobs = before.filter(isMigrateJob);
  const newJobs = after.filter(isMigrateJob);
  let normalized = after;
  let migrateJob: ReleaseChange["migrateJob"];
  const [oldJob] = oldJobs;
  const [newJob] = newJobs;
  if (oldJobs.length !== newJobs.length || oldJobs.length > 1) {
    problems.push(`the release has ${oldJobs.length} migration Job(s) before and ${newJobs.length} after, expected one or none both times`);
  } else if (oldJob !== undefined && newJob !== undefined && oldJob.metadata?.name !== newJob.metadata?.name) {
    const oldName = oldJob.metadata?.name ?? "";
    const newName = newJob.metadata?.name ?? "";
    migrateJob = { before: oldName, after: newName };
    // Back to the old name everywhere, and the gateway's entry back to its old image.
    normalized = JSON.parse(JSON.stringify(after).replaceAll(newName, oldName)) as K8sObject[];
    const job = normalized.find(isMigrateJob);
    const was = serviceImages(podAnnotations(oldJob)[SERVICE_IMAGES_ANNOTATION]);
    const annotations = job === undefined ? {} : podAnnotations(job);
    const now = serviceImages(annotations[SERVICE_IMAGES_ANNOTATION]);
    const others = [...new Set([...was.keys(), ...now.keys()])].filter((name) => name !== AI_GATEWAY && was.get(name) !== now.get(name));
    if (now.get(AI_GATEWAY) !== image || others.length > 0) {
      problems.push(`the migration Job's ${SERVICE_IMAGES_ANNOTATION} names ${String(now.get(AI_GATEWAY))} for ${AI_GATEWAY}${others.length > 0 ? ` and changed for ${others.join(", ")}` : ""}, expected only ${AI_GATEWAY}=${image}`);
    } else {
      annotations[SERVICE_IMAGES_ANNOTATION] = podAnnotations(oldJob)[SERVICE_IMAGES_ANNOTATION];
    }
  }
  const waiting: string[] = [];
  const old = new Map(before.map((o) => [keyOf(o), o]));
  const now = new Map(normalized.map((o) => [keyOf(o), o]));
  // The workloads as rendered, before the Job name went back: the ones the Job name alone changed.
  const raw = new Map(after.map((o) => [keyOf(o), o]));
  for (const [key, o] of old) if (!now.has(key)) problems.push(`${nameOf(o)} is gone`);
  for (const [key, o] of now) if (!old.has(key)) problems.push(`${nameOf(o)} is new`);
  let gateways = 0;
  for (const [key, o] of now) {
    const previous = old.get(key);
    if (previous === undefined) continue;
    if (isAiGateway(o)) {
      gateways += 1;
      const container = aiGatewayContainer(o);
      if (container?.image !== image || container.imagePullPolicy !== "Never") {
        problems.push(`${nameOf(o)} runs ${String(container?.image)} with pull policy ${String(container?.imagePullPolicy)}, not ${image} with Never`);
        continue;
      }
      const restored = structuredClone(o);
      const target = aiGatewayContainer(restored);
      const original = aiGatewayContainer(previous);
      if (target !== undefined && original !== undefined) {
        target.image = original.image;
        target.imagePullPolicy = original.imagePullPolicy;
      }
      if (JSON.stringify(restored) !== JSON.stringify(previous)) problems.push(`${nameOf(o)} changed beyond its container's image and pull policy`);
      continue;
    }
    if (JSON.stringify(o) !== JSON.stringify(previous)) problems.push(`${nameOf(o)} changed`);
    else if (WORKLOAD_KINDS.has(o.kind) && JSON.stringify(raw.get(key)) !== JSON.stringify(previous)) waiting.push(nameOf(o));
  }
  if (gateways !== 1) problems.push(`the release has ${gateways} ai-gateway Deployments, expected 1`);
  return { problems, ...(migrateJob === undefined ? {} : { migrateJob }), waiting };
}

/** releaseChange's problems alone. */
export const releaseChangeProblems = (before: K8sObject[], after: K8sObject[], image = AI_GATEWAY_DOUBLE_IMAGE): string[] => releaseChange(before, after, image).problems;

/** The pods of a component as `kubectl get pods --output json` lists them: image of its container, ready, terminating. */
export function componentPods(podList: unknown, container: string): { name: string; image: string; ready: boolean; terminating: boolean }[] {
  return asArray(asMap(podList).items)
    .filter(isMap)
    .map((pod) => {
      const metadata = asMap(pod.metadata);
      const spec = asArray(asMap(pod.spec).containers).filter(isMap).find((c) => c.name === container);
      const ready = asArray(asMap(pod.status).conditions)
        .filter(isMap)
        .some((c) => c.type === "Ready" && c.status === "True");
      return { name: asString(metadata.name) ?? "", image: asString(spec?.image) ?? "", ready, terminating: metadata.deletionTimestamp !== undefined };
    });
}

export interface RolloutDeps {
  /** `kubectl get pods --selector <the ai-gateway's labels> --output json`, parsed. */
  pods: () => Promise<unknown>;
  sleep?: (ms: number) => Promise<void>;
  now?: () => number;
  limits?: { rolloutMs?: number; pollMs?: number };
}

/**
 * Waits until every ai-gateway pod runs the stand-in and is ready, and no pod of the old image is
 * left, terminating ones included: the questions then reach the stand-in only, and its log is the
 * only one the check reads. Returns the report part.
 */
export async function waitForAiGatewayDouble(deps: RolloutDeps, image = AI_GATEWAY_DOUBLE_IMAGE): Promise<string> {
  const found = await pollUntil(
    `every ${AI_GATEWAY} pod to run ${image} and be ready, with no pod of another image left`,
    deps.limits?.rolloutMs ?? ROLLOUT_LIMIT_MS,
    deps.limits?.pollMs ?? POLL_MS,
    deps,
    async () => {
      const pods = componentPods(await deps.pods(), AI_GATEWAY);
      const others = pods.filter((p) => p.image !== image);
      const ready = pods.filter((p) => p.image === image && p.ready && !p.terminating);
      if (pods.length > 0 && others.length === 0 && ready.length === pods.length) return { done: pods.length };
      const seen = pods.map((p) => `${p.name} ${p.image}${p.ready ? " ready" : ""}${p.terminating ? " terminating" : ""}`).join(", ");
      return { pending: seen === "" ? "no pod" : seen };
    },
  );
  return `${found.value} ${AI_GATEWAY} pod(s) run ${image} and are ready, no other pod left after ${Math.round(found.waitedMs / 1000)} s`;
}

// ---------------------------------------------------------------------------------------------
// The data.

/** One tenant's data for the run; the canaries are random to the run. */
export interface TenantData {
  /** "A" or "B", for the messages. */
  label: string;
  /** The device of the readings, random. */
  device: string;
  /** The device model's name (`^[a-z][a-z0-9]*(-[a-z0-9]+)*$`, at most 40 characters). */
  modelName: string;
  /** The text reading. */
  text: string;
  /** The two numeric readings, a minute apart: a counter that rose by `last - first`. */
  first: number;
  last: number;
}

function hex(bytes: number): string {
  return Buffer.from(crypto.getRandomValues(new Uint8Array(bytes))).toString("hex");
}

function randomInt(min: number, max: number): number {
  const [value] = crypto.getRandomValues(new Uint32Array(1));
  return min + ((value ?? 0) % (max - min + 1));
}

/**
 * A tenant's data with fresh canaries. The numbers are exact in binary, so every service prints
 * them the way JavaScript does: the first reading ends in .625, the rise in .25 (an even integer
 * part, so the average of the two ends in .75 and is no other canary's tail).
 */
export function newTenantData(label: string): TenantData {
  const first = randomInt(100_000, 899_999) + 0.625;
  const rise = 2 * randomInt(500, 4_999) + 0.25;
  return { label, device: crypto.randomUUID(), modelName: `nightly-m-${hex(8)}`, text: `nightly-t-${hex(8)}`, first, last: first + rise };
}

/** The usage telemetry-query computes for the two readings, and their average. */
export const riseOf = (data: TenantData): number => data.last - data.first;
export const averageOf = (data: TenantData): number => (data.first + data.last) / 2;

/** Every text by which the tenant's data is recognized: the model name, the text reading and the numbers as printed. */
export function canariesOf(data: TenantData): string[] {
  return [data.modelName, data.text, ...[data.first, data.last, riseOf(data), averageOf(data)].map(String)];
}

/** Canaries of one tenant that hold, or are held in, a canary of the other: they would not tell the tenants apart. */
export function canaryClashes(a: TenantData, b: TenantData): string[] {
  const clashes: string[] = [];
  for (const x of canariesOf(a)) for (const y of canariesOf(b)) if (x.includes(y) || y.includes(x)) clashes.push(`${x} and ${y}`);
  return clashes;
}

/** Data for tenants A and B whose canaries tell them apart. */
export function tenantPair(make: (label: string) => TenantData = newTenantData): { a: TenantData; b: TenantData } {
  for (let attempt = 0; attempt < 100; attempt++) {
    const a = make("A");
    const b = make("B");
    if (canaryClashes(a, b).length === 0) return { a, b };
  }
  throw new Error("no pair of tenant data whose canaries tell the tenants apart in 100 attempts");
}

/** `2026-11-04 10:00:00.000`: a DateTime64(3, 'UTC') literal. */
function clickhouseTime(at: Date): string {
  const text = at.toISOString();
  if (!/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/.test(text)) throw new Error(`not a time ClickHouse reads: ${text}`);
  return text.replace("T", " ").replace("Z", "");
}

function numberLiteral(value: number): string {
  const text = String(value);
  if (!Number.isFinite(value) || !/^\d{1,9}(\.\d{1,6})?$/.test(text)) throw new Error(`not a reading the check writes: ${text}`);
  return text;
}

/**
 * The INSERT of one tenant's readings, one batch of one stream, as ingest writes them
 * (services/ingest/migrations/clickhouse/0002_telemetry.sql and 0012): the numbers at `firstAt`
 * and a minute later, and the text at the later minute. Built only from values it checks (UUIDs,
 * the fixed points and gateway, the canaries' shapes, finite numbers), so nothing else reaches
 * the statement.
 */
export function telemetryRowsSql(tenantId: string, data: TenantData, stream: string, firstAt: Date, receivedAt: Date): string {
  const tenant = uuid(tenantId);
  const device = uuid(data.device);
  if (!/^nightly-[0-9a-f]{16}$/.test(stream)) throw new Error(`not a stream ID the check writes: ${stream}`);
  if (!/^nightly-t-[0-9a-f]{16}$/.test(data.text)) throw new Error(`not a text reading the check writes: ${data.text}`);
  const later = new Date(firstAt.getTime() + 60_000);
  const row = (point: string, at: Date, type: "float" | "string", float: string, text: string) =>
    `('${tenant}', '${GATEWAY_ID}', '${stream}', 1, '${device}', '${point}', '${clickhouseTime(at)}', '${clickhouseTime(receivedAt)}', '${type}', ${float}, '${text}')`;
  return [
    "INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id, batch_seq, device_id, point, ts, received_at, value_type, value_float, value_string) VALUES",
    [
      row(NUMBER_POINT, firstAt, "float", numberLiteral(data.first), ""),
      row(NUMBER_POINT, later, "float", numberLiteral(data.last), ""),
      row(TEXT_POINT, later, "string", "0", data.text),
    ].join(",\n"),
    "",
  ].join("\n");
}

/** RFC 3339 in UTC, to the second. */
const rfc3339 = (at: Date): string => at.toISOString().replace(/\.\d{3}Z$/, "Z");

/** When the readings are (three minutes before the current minute) and the range the tools ask for around them. */
export function readingTimes(nowMs: number): { firstAt: Date; from: string; to: string } {
  const minute = Math.floor(nowMs / 60_000) * 60_000;
  const firstAt = new Date(minute - 3 * 60_000);
  return { firstAt, from: rfc3339(new Date(minute - 5 * 60_000)), to: rfc3339(new Date(minute + 2 * 60_000)) };
}

// ---------------------------------------------------------------------------------------------
// The directive.

export interface ToolCall {
  name: string;
  input: Record<string, unknown>;
}

/** A question that makes the stand-in call `calls` (ai-gateway-double/server.ts). */
export function directive(tag: string, calls: ToolCall[]): string {
  if (!TAG.test(tag)) throw new Error(`not a directive tag: ${tag}`);
  if (calls.length < 1 || calls.length > 8) throw new Error(`a directive has 1 to 8 calls, not ${calls.length}`);
  return `${DIRECTIVE_PREFIX}${JSON.stringify({ tag, calls })}`;
}

/** Every telemetry tool on `device`, over [from, to) in UTC. */
export function telemetryCalls(device: string, from: string, to: string): ToolCall[] {
  const series = (points: string[]) => points.map((point) => ({ device_id: uuid(device), point }));
  return [
    { name: "telemetry_latest", input: { device_id: uuid(device) } },
    { name: "telemetry_statistics", input: { series: series([NUMBER_POINT, TEXT_POINT]), tz: "UTC", from, to } },
    { name: "telemetry_usage", input: { series: series([NUMBER_POINT]), tz: "UTC", from, to } },
    { name: "telemetry_sql", input: { sql: `SELECT point, value_type, value_float, value_string FROM tenant_telemetry WHERE device_id = '${uuid(device)}' ORDER BY point, ts` } },
  ];
}

/** What every text reading starts with (newTenantData), in both tenants. */
const TEXT_PREFIX = "nightly-t-";
/** telemetry_sql with no device filter: every text reading of the run that the caller's tenant view holds. */
export const PREFIX_SQL = `SELECT point, value_string FROM tenant_telemetry WHERE startsWith(value_string, '${TEXT_PREFIX}') ORDER BY ts`;
/**
 * telemetry_sql on the base table, which holds every tenant's rows. The SQL check reads the
 * tenant views only (services/telemetry-query/src/sql/check.rs, VIEWS) and refuses any other
 * table (SqlRefusal::TableNotAllowed, a 422 refusal), which the tool reports as input_error
 * (services/assistant/src/tools/telemetry_sql.rs, Posted::Refused).
 */
export const BASE_TABLE_SQL = `SELECT point, value_string FROM trustiot.telemetry WHERE startsWith(value_string, '${TEXT_PREFIX}')`;

/** What one call of a directive must give the model. */
export type Expected =
  /** ok, and the result holds these texts of the asker's own data. */
  | { kind: "holds"; texts: string[] }
  /** ok, and the result is empty (resultShape). */
  | { kind: "empty" }
  /** input_error: telemetry-query refused the input. */
  | { kind: "refused" };

/** One call of a directive, its name in the messages, and what it must give the model. */
export interface PlannedCall {
  call: ToolCall;
  label: string;
  expect: Expected;
}

/** The outcome the assistant logs for a call that gives the model what `expected` says. */
export const outcomeOf = (expected: Expected): string => (expected.kind === "refused" ? "input_error" : "ok");

/** Check 1: device_models and every telemetry tool on the asker's own device; each result holds the asker's data. */
export function ownPlan(asker: TenantData, from: string, to: string): PlannedCall[] {
  return [{ name: MODEL_TOOL, input: {} }, ...telemetryCalls(asker.device, from, to)].map((call) => ({ call, label: call.name, expect: { kind: "holds", texts: ownCanaries(call.name, asker) } }));
}

/**
 * Check 2 (`asker` A, `owner` B) and its control (both B): every telemetry tool on the owner's
 * device, telemetry_sql with no device filter and telemetry_sql on the base table. As the owner,
 * each telemetry tool's result holds the owner's data. As another tenant, each is ok and empty:
 * telemetry-query answers a device of another tenant with 200 and no data, never 403 or 404 (it
 * reads with the caller's tenant: services/telemetry-query/src/history.rs, get_latest;
 * src/semantic/mod.rs, post_statistics and post_usage, whose series then say no_data;
 * src/sql_api.rs, run_sql, on the caller's tenant view), so a denied call would mean that the
 * token did not act as the asker. The SQL without a filter holds the asker's own text reading and
 * no other; the one on the base table is refused.
 */
export function crossPlan(asker: TenantData, owner: TenantData, from: string, to: string): PlannedCall[] {
  const own = asker.label === owner.label;
  const telemetry: PlannedCall[] = telemetryCalls(owner.device, from, to).map((call) => ({
    call,
    label: `${call.name} (${owner.label}'s device)`,
    expect: own ? { kind: "holds", texts: ownCanaries(call.name, owner) } : { kind: "empty" },
  }));
  return [
    ...telemetry,
    { call: { name: "telemetry_sql", input: { sql: PREFIX_SQL } }, label: "telemetry_sql (no device filter)", expect: { kind: "holds", texts: [asker.text] } },
    { call: { name: "telemetry_sql", input: { sql: BASE_TABLE_SQL } }, label: "telemetry_sql (trustiot.telemetry)", expect: { kind: "refused" } },
  ];
}

// ---------------------------------------------------------------------------------------------
// Logs.

/** One logged event: a service's (its OTLP record) or the stand-in's (one of its lines). */
export interface LogEvent {
  /** The service that logged it (OTEL_SERVICE_NAME, its key under `services`), or AI_GATEWAY for the stand-in. */
  service: string;
  message: string;
  fields: Json;
}

/** Where the stand-in serves the lines of one directive tag (ai-gateway-double/server.ts, LINES_PATH). */
export const STAND_IN_LINES_PATH = "/nightly/v1/lines";
/**
 * The port it serves them on (ai-gateway-double/server.ts, LINES_PORT's default): bound to the
 * pod's loopback and declared nowhere in the chart, so only `kubectl port-forward`, which connects
 * to localhost inside the pod, reaches it.
 */
export const STAND_IN_LINES_PORT = 8083;

/** The stand-in's lines as STAND_IN_LINES_PATH serves them; lines without an event are skipped. */
export function standInEvents(lines: readonly unknown[]): LogEvent[] {
  return lines.filter(isMap).flatMap((line) => (typeof line.event === "string" ? [{ service: AI_GATEWAY, message: line.event, fields: line }] : []));
}

/** The table the OpenTelemetry collector writes the services' logs into (services/ingest/migrations/clickhouse/0006_logs.sql). */
export const LOG_TABLE = "trustiot_logs.otel_logs";
/** The events the check reads from it. */
export const SERVICE_EVENTS = [RUN_STARTED, TOOL_CALL_COMPLETED, DELEGATION_ISSUED, DELEGATION_EXCHANGED] as const;

/**
 * The query for the services' events of `runs`: the assistant names a run `run_id`, the identity
 * service `run`. Only the event's service, message and fields, one JSON object per line. Built
 * from the fixed events and checked UUIDs only.
 */
export function serviceEventsSql(runs: readonly string[]): string {
  if (runs.length === 0) throw new Error("no run to read the events of");
  const ids = runs.map((id) => `'${uuid(id)}'`).join(", ");
  return [
    "SELECT ServiceName AS service, Body AS message, LogAttributes AS fields",
    `FROM ${LOG_TABLE}`,
    `WHERE Body IN (${SERVICE_EVENTS.map((event) => `'${event}'`).join(", ")})`,
    `  AND (LogAttributes['run_id'] IN (${ids}) OR LogAttributes['run'] IN (${ids}))`,
    "ORDER BY Timestamp",
    "FORMAT JSONEachRow",
    "",
  ].join("\n");
}

/** What serviceEventsSql printed; a line that is not such an event fails, since the query makes no other. */
export function parseServiceEvents(text: string): LogEvent[] {
  return text
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(line);
      } catch {
        throw new Error(`a line of the ${LOG_TABLE} query is not JSON (${line.length} characters)`);
      }
      const row = asMap(parsed);
      if (typeof row.service !== "string" || typeof row.message !== "string" || !isMap(row.fields)) {
        throw new Error(`a line of the ${LOG_TABLE} query is not an event (keys ${Object.keys(row).sort().join(",")})`);
      }
      return { service: row.service, message: row.message, fields: row.fields };
    });
}

/** The stand-in's line for one tool_result block. */
export interface ToolResultLine {
  tenant: string;
  tool: string;
  toolUseId: string;
  isError: boolean;
  text: string;
}

/** The stand-in's line for the model call of a question. */
export interface RequestLine {
  tenant: string;
  offered: string[];
  requested: string[];
  skipped: string[];
}

/** What the stand-in's log says of the question tagged `tag`, once both of its model calls are there. */
export function standInRecord(events: LogEvent[], tag: string): { question: RequestLine; resultsTenant: string; results: ToolResultLine[] } | undefined {
  const strings = (value: unknown) => asArray(value).filter((v): v is string => typeof v === "string");
  const request = (turn: string) => events.find((e) => e.message === "request" && e.fields.tag === tag && e.fields.turn === turn);
  const question = request("question");
  if (question === undefined) return undefined;
  const line: RequestLine = {
    tenant: asString(question.fields.tenant) ?? "",
    offered: strings(question.fields.offered),
    requested: strings(question.fields.requested),
    skipped: strings(question.fields.skipped),
  };
  // Every requested tool was skipped: the stand-in answered at once, with no second call.
  if (line.requested.length > 0 && line.requested.every((tool) => line.skipped.includes(tool))) return { question: line, resultsTenant: line.tenant, results: [] };
  const answered = request("tool_results");
  if (answered === undefined) return undefined;
  const results = events
    .filter((e) => e.message === "tool_result" && e.fields.tag === tag)
    .map((e) => ({
      tenant: asString(e.fields.tenant) ?? "",
      tool: asString(e.fields.tool) ?? "",
      toolUseId: asString(e.fields.tool_use_id) ?? "",
      isError: e.fields.is_error === true,
      text: asString(e.fields.text) ?? "",
    }));
  return { question: line, resultsTenant: asString(answered.fields.tenant) ?? "", results };
}

/** The assistant's `tool call completed` events of `run`: tool, outcome and tenant. */
export function toolCallsOf(events: LogEvent[], run: string): { tool: string; outcome: string; tenant: string }[] {
  return events
    .filter((e) => e.service === "assistant" && e.message === TOOL_CALL_COMPLETED && e.fields.run_id === run)
    .map((e) => ({ tool: asString(e.fields.tool) ?? "", outcome: asString(e.fields.outcome) ?? "", tenant: asString(e.fields.tenant_id) ?? "" }));
}

// ---------------------------------------------------------------------------------------------
// What a tool returned.

/**
 * Whether a tool's result is empty: no row, or every series without data (telemetry-query's
 * answer for readings the caller's tenant does not have), and its shape for a message (keys,
 * statuses, counts; never a value).
 */
export function resultShape(tool: string, text: string): { empty: boolean; shape: string } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return { empty: false, shape: `not JSON (${text.length} characters)` };
  }
  const result = asMap(parsed);
  const keys = Object.keys(result).sort().join(",");
  if (tool === "telemetry_latest" || tool === MODEL_TOOL) {
    const rows = asArray(result.rows).length;
    return { empty: rows === 0 && result.total === 0, shape: `{${keys}} with ${rows} row(s), total ${String(result.total)}` };
  }
  if (tool === "telemetry_statistics" || tool === "telemetry_usage") {
    const statuses = asArray(result.series).map((s) => String(asMap(s).status));
    return { empty: statuses.length > 0 && statuses.every((s) => s === "no_data"), shape: `{${keys}} with series ${statuses.join(",") || "none"}` };
  }
  if (tool === "telemetry_sql") {
    const rows = asArray(result.rows).length;
    return { empty: rows === 0 && result.row_count === 0, shape: `{${keys}} with ${rows} row(s), row_count ${String(result.row_count)}` };
  }
  return { empty: false, shape: `{${keys}}` };
}

/** The canaries of `canaries` that `text` holds. */
export const leaksIn = (text: string, canaries: readonly string[]): string[] => canaries.filter((canary) => text.includes(canary));

/** The texts of A's data that each tool's result on A's device must hold. */
export function ownCanaries(tool: string, data: TenantData): string[] {
  switch (tool) {
    case MODEL_TOOL:
      return [data.modelName];
    case "telemetry_latest":
      return [data.text, String(data.last)];
    case "telemetry_statistics":
      return [String(data.first), String(data.last)];
    case "telemetry_usage":
      return [String(riseOf(data))];
    case "telemetry_sql":
      return [data.text, String(data.first), String(data.last)];
    default:
      return [];
  }
}

// ---------------------------------------------------------------------------------------------
// The check.

/** What the check needs from the cluster; tests replace it, so no cluster is needed. */
export interface ToolsCheckDeps {
  /** Sends one request to the ingress gateway. */
  send: (request: RawRequest) => Promise<RawResponse>;
  /** Runs SQL as the database superuser; the SQL and what it printed stay out of the log. */
  psql: (sql: string) => Promise<string>;
  /**
   * Runs a statement as the bundled ClickHouse's administrator in its pod and returns what it
   * printed: the readings' INSERT and the services' events (serviceEventsSql). The statement and
   * the answer stay out of the log.
   */
  clickhouse: (sql: string) => Promise<string>;
  /** The stand-in's lines of one directive tag, from every ai-gateway pod (STAND_IN_LINES_PATH); never written to the run log. */
  standInLines: (tag: string) => Promise<unknown[]>;
  log: (line: string) => void;
  sleep?: (ms: number) => Promise<void>;
  now?: () => number;
  limits?: { tenantMs?: number; answerMs?: number; logMs?: number; pollMs?: number };
  /** The tenants' data (tenantPair unless a test fixes it). */
  data?: { a: TenantData; b: TenantData };
  /** A fresh directive tag (16 hex digits unless a test fixes them). */
  tag?: () => string;
}

const cookieOf = (session: TestSession): [string, string] => ["cookie", `${SESSION_COOKIE}=${session.cookie}`];

/** Status and ErrorBody code and key, never the body itself. */
function said(response: RawResponse): string {
  const code = errorCode(response.body);
  const key = errorMessageKey(response.body);
  return [String(response.status), code, key].filter((part) => part !== undefined).join(" ");
}

function parsedJson(step: string, status: number, body: string): Json {
  try {
    const parsed: unknown = JSON.parse(body);
    if (isMap(parsed)) return parsed;
  } catch {
    // Reported below.
  }
  throw new Error(`${step} answered ${status} with a body that is not a JSON object`);
}

const parsedBody = (step: string, response: RawResponse): Json => parsedJson(step, response.status, response.body);

/** One member, the tenant it acts in and the data its tenant holds. */
interface Member {
  session: TestSession;
  tenantId: string;
  data: TenantData;
}

/** A question's run as the conversation shows it once it ended. */
interface RunAnswer {
  runId: string;
  status: string;
  answer: string;
  removed: string[];
  waitedMs: number;
}

/** What the logs say of a question's tool calls: the stand-in's record, the tools it called, and the assistant's lines. */
interface ToolRecord {
  record: NonNullable<ReturnType<typeof standInRecord>>;
  called: string[];
  calls: ReturnType<typeof toolCallsOf>;
}

class ToolsCheck {
  private readonly pollMs: number;

  constructor(
    private readonly deps: ToolsCheckDeps,
    private readonly appHost: string,
  ) {
    this.pollMs = deps.limits?.pollMs ?? POLL_MS;
  }

  get(target: string, session: TestSession): RawRequest {
    return { target, hostHeader: this.appHost, headers: [cookieOf(session)] };
  }

  post(target: string, body: unknown, session: TestSession): RawRequest {
    return { target, method: "POST", hostHeader: this.appHost, headers: [cookieOf(session), ["content-type", "application/json"]], body: JSON.stringify(body) };
  }

  /** Polls `request` until `verdict` says done; a lost connection is pending. */
  async poll<T>(what: string, limitMs: number, request: () => RawRequest, verdict: (r: RawResponse) => { done: T } | { pending: string } | { fail: string }) {
    return pollUntil<T>(what, limitMs, this.pollMs, this.deps, async () => {
      try {
        return verdict(await this.deps.send(request()));
      } catch (error) {
        if (error instanceof ConnectionError) return { pending: error.message };
        throw error;
      }
    });
  }

  /** A tenant made by the platform administrator; sent once, since a retry could make a second one. */
  async tenant(admin: TestSession, name: string): Promise<string> {
    const created = await this.deps.send(this.post(TENANTS_PATH, { name }, admin));
    if (created.status !== 201) throw new Error(`creating tenant "${name}" answered ${said(created)}, expected 201`);
    const id = asString(parsedBody(`creating tenant "${name}"`, created).tenant_id);
    if (id === undefined || !UUID.test(id)) throw new Error(`creating tenant "${name}" answered 201 without a tenant ID`);
    return id;
  }

  /**
   * The member's device model, once the assets service knows the tenant: until then it answers
   * 503 and writes nothing, so the POST is sent again. After a lost answer the POST may have
   * created the model, so a 409 for its name then counts as made; the lists checked next show
   * whether it is there.
   */
  async deviceModel(member: Member): Promise<number> {
    const name = member.data.modelName;
    let lost = false;
    const made = await pollUntil<true>(
      `the assets service to create device model ${name} for tenant ${member.data.label}'s editor (201)`,
      this.deps.limits?.tenantMs ?? TENANT_READY_LIMIT_MS,
      this.pollMs,
      this.deps,
      async () => {
        let r: RawResponse;
        try {
          r = await this.deps.send(this.post(DEVICE_MODELS_PATH, { name, points: [] }, member.session));
        } catch (error) {
          if (!(error instanceof ConnectionError)) throw error;
          lost = true;
          return { pending: error.message };
        }
        if (r.status === 201) return { done: true };
        if (lost && r.status === 409 && errorMessageKey(r.body) === "device_model.name_taken") return { done: true };
        if (r.status === 503) return { pending: said(r) };
        return { fail: `answered ${said(r)}` };
      },
    );
    return made.waitedMs;
  }

  /** The names of the device models the member lists. */
  async modelNames(member: Member): Promise<string[]> {
    const listed = await this.deps.send(this.get(DEVICE_MODELS_PATH, member.session));
    if (listed.status !== 200) throw new Error(`GET ${DEVICE_MODELS_PATH} as tenant ${member.data.label}'s editor answered ${said(listed)}, expected 200`);
    return asArray(parsedBody(`GET ${DEVICE_MODELS_PATH}`, listed).models).map((m) => asString(asMap(m).name) ?? "");
  }

  /** The member's GET of a device's latest readings: the body once telemetry-query has the tenant's account. */
  async latest(member: Member, device: string): Promise<{ body: string; points: number; waitedMs: number }> {
    const target = telemetryLatestPath(device);
    const found = await this.poll<string>(
      `telemetry-query to answer tenant ${member.data.label}'s editor with its ClickHouse account (200)`,
      this.deps.limits?.tenantMs ?? TENANT_READY_LIMIT_MS,
      () => this.get(target, member.session),
      (r) => {
        if (r.status === 200) return { done: r.body };
        if ((r.status === 404 && errorMessageKey(r.body) === ACCOUNT_NOT_READY) || r.status === 503) return { pending: said(r) };
        return { fail: `answered ${said(r)}` };
      },
    );
    return { body: found.value, points: asArray(parsedJson(`GET ${target}`, 200, found.value).points).length, waitedMs: found.waitedMs };
  }

  /** Asks `question` in a new conversation of the member and waits for its run to end. */
  async ask(member: Member, question: string): Promise<RunAnswer> {
    const started = await this.deps.send(this.post(ASSISTANT_CONVERSATIONS_PATH, {}, member.session));
    if (started.status !== 201) throw new Error(`POST ${ASSISTANT_CONVERSATIONS_PATH} as tenant ${member.data.label}'s editor answered ${said(started)}, expected 201`);
    const conversation = asString(parsedBody(`POST ${ASSISTANT_CONVERSATIONS_PATH}`, started).id);
    if (conversation === undefined || !UUID.test(conversation)) throw new Error(`POST ${ASSISTANT_CONVERSATIONS_PATH} answered 201 without a conversation ID`);
    const detail = assistantConversationPath(conversation);
    const questions = QUESTIONS_PATH.replace("{conversation_id}", conversation);

    // Sent again only while the replica is busy (nothing started) or when the answer was lost and
    // the conversation shows no run (nothing started either).
    const asked = await pollUntil<string>(
      `the assistant to start the question in conversation ${conversation} (202)`,
      this.deps.limits?.answerMs ?? ANSWER_LIMIT_MS,
      this.pollMs,
      this.deps,
      async () => {
        let response: RawResponse;
        try {
          response = await this.deps.send(this.post(questions, { question }, member.session));
        } catch (error) {
          if (!(error instanceof ConnectionError)) throw error;
          let shown: RawResponse;
          try {
            shown = await this.deps.send(this.get(detail, member.session));
          } catch (again) {
            if (again instanceof ConnectionError) return { pending: again.message };
            throw again;
          }
          const run = asString(asMap(shown.status === 200 ? parsedBody(`GET ${detail}`, shown).latest_run : undefined).id);
          return run === undefined ? { pending: error.message } : { done: run };
        }
        if (response.status === 202) {
          const run = asString(parsedBody(`POST ${QUESTIONS_PATH}`, response).id);
          return run !== undefined && UUID.test(run) ? { done: run } : { fail: "answered 202 without a run ID" };
        }
        if (response.status === 503 && errorMessageKey(response.body) === "question.busy") return { pending: said(response) };
        return { fail: `answered ${said(response)}` };
      },
    );
    const runId = asked.value;

    const ended = await this.poll<Json>(
      `run ${runId} of conversation ${conversation} to end (latest_run.status other than running)`,
      this.deps.limits?.answerMs ?? ANSWER_LIMIT_MS,
      () => this.get(detail, member.session),
      (r) => {
        if (r.status !== 200) return { fail: `GET ${ASSISTANT_CONVERSATIONS_PATH}/{id} answered ${said(r)}` };
        const shown = parsedBody(`GET ${detail}`, r);
        const latest = asMap(shown.latest_run);
        if (latest.id !== runId) return { fail: `latest_run is ${String(latest.id)}, not run ${runId}` };
        if (latest.status === "running") return { pending: "running" };
        return { done: shown };
      },
    );
    const latest = asMap(ended.value.latest_run);
    const status = asString(latest.status) ?? "";
    if (status !== "answered") {
      throw new Error(`run ${runId} ended ${status} (result ${String(latest.result)}, message_key ${String(latest.message_key)}), expected answered`);
    }
    const answer = asArray(ended.value.messages).filter(isMap).at(-1);
    if (answer?.kind !== "answer" || typeof answer.text !== "string") throw new Error(`run ${runId} was answered, but the conversation's last message is no answer`);
    const removed = asArray(answer.removed).map(String);
    return { runId, status, answer: answer.text, removed, waitedMs: asked.waitedMs + ended.waitedMs };
  }

  /** The services' events of `runs` in the log table. */
  async serviceEvents(runs: readonly string[]): Promise<LogEvent[]> {
    return parseServiceEvents(await this.deps.clickhouse(serviceEventsSql(runs)));
  }

  /** Polls the logs until the stand-in's record of `tag` and the assistant's tool calls of `run` are complete. */
  async toolRecord(tag: string, run: string, member: Member) {
    return pollUntil<ToolRecord>(
      `the stand-in's lines of question ${tag} and the assistant's "${TOOL_CALL_COMPLETED}" lines of run ${run}`,
      this.deps.limits?.logMs ?? LOG_LIMIT_MS,
      this.pollMs,
      this.deps,
      async () => {
        let lines: unknown[];
        try {
          lines = await this.deps.standInLines(tag);
        } catch (error) {
          if (error instanceof ConnectionError) return { pending: error.message };
          throw error;
        }
        const record = standInRecord(standInEvents(lines), tag);
        if (record === undefined) return { pending: `no complete record of question ${tag} in the stand-in's log` };
        const called = record.question.requested.filter((tool) => !record.question.skipped.includes(tool));
        const calls = toolCallsOf(await this.serviceEvents([run]), run);
        if (calls.length < called.length || record.results.length < called.length) {
          return { pending: `${calls.length} tool call line(s) of the assistant and ${record.results.length} result line(s) of the stand-in, for ${called.length} call(s)` };
        }
        for (const line of [record.question.tenant, record.resultsTenant, ...record.results.map((r) => r.tenant), ...calls.map((c) => c.tenant)]) {
          if (line !== member.tenantId) return { fail: `a line of the run names tenant ${line}, not tenant ${member.data.label} (${member.tenantId})` };
        }
        return { done: { record, called, calls } };
      },
    );
  }
}

/** Tag of a directive: 16 hex digits. */
const newTag = (): string => hex(8);

/** The steps of the check, each with one evidence line `evidence: <shape> <step>: …`, in order. */
export const EVIDENCE_STEPS = ["setup", "check 1", "control", "check 2", "check 3"] as const;

/** The tool use ID the stand-in gives call `index` of the directive tagged `tag` (ai-gateway-double/server.ts). */
const toolUseId = (tag: string, index: number): string => `toolu_nightly_${tag}_${index}`;

/**
 * Checks what the model got for each planned call of the question tagged `tag` (the stand-in's
 * tool_result line of the call, by its tool use ID) and what the assistant logged of each tool
 * (its outcomes, compared per tool as a multiset: the calls of a step run at once and the log
 * line names no call). `asker` is the label of the tenant whose data the results may hold;
 * `foreign` the other tenant's canaries, which no result may hold. Returns one part per call for
 * the evidence line; throws naming the step, the call and the outcome or the result's shape.
 */
function checkPlan(step: string, plan: PlannedCall[], tag: string, found: ToolRecord, asker: string, foreign: { label: string; canaries: string[] }): string[] {
  const { question, results } = found.record;
  if (question.skipped.length > 0) {
    throw new Error(`${step}: the smoke's assistant does not offer ${question.skipped.join(", ")} to the model, though the check needs every tool (registeredTools)`);
  }
  const resultOf = plan.map((_, index) => results.find((r) => r.toolUseId === toolUseId(tag, index)));
  // Another tenant's data first: the worst failure, reported whatever else is wrong.
  for (const [index, planned] of plan.entries()) {
    const result = resultOf[index];
    if (result === undefined) continue;
    const leaked = leaksIn(result.text, foreign.canaries);
    if (leaked.length > 0) throw new Error(`${step}: ${planned.label} handed the model tenant ${foreign.label}'s ${leaked.join(", ")} (${resultShape(planned.call.name, result.text).shape})`);
  }
  const tools = [...new Set(plan.map((p) => p.call.name))];
  for (const tool of tools) {
    const expected = plan.filter((p) => p.call.name === tool).map((p) => outcomeOf(p.expect)).sort();
    const logged = found.calls.filter((c) => c.tool === tool).map((c) => c.outcome).sort();
    if (logged.join() !== expected.join()) throw new Error(`${step}: the assistant logged ${tool} ${logged.join(", ") || "never"}, expected ${expected.join(", ")}`);
  }
  const unplanned = found.calls.filter((c) => !tools.includes(c.tool)).map((c) => c.tool);
  if (unplanned.length > 0) throw new Error(`${step}: the assistant logged calls of ${[...new Set(unplanned)].join(", ")}, which the directive does not name`);
  const parts: string[] = [];
  for (const [index, planned] of plan.entries()) {
    const result = resultOf[index];
    if (result === undefined) throw new Error(`${step}: the model got no result of ${planned.label}`);
    const shape = resultShape(planned.call.name, result.text);
    const expect = planned.expect;
    if (expect.kind === "holds") {
      if (result.isError) throw new Error(`${step}: ${planned.label} gave the model an error result (${shape.shape}), expected ok`);
      const missing = expect.texts.filter((text) => !result.text.includes(text));
      if (missing.length > 0) throw new Error(`${step}: ${planned.label}'s result (${shape.shape}) lacks tenant ${asker}'s ${missing.join(", ")}`);
      parts.push(`${planned.label} ok (${shape.shape})`);
    } else if (expect.kind === "empty") {
      if (result.isError || !shape.empty) throw new Error(`${step}: ${planned.label} gave the model ${result.isError ? "an error result" : "a result that is not empty"} (${shape.shape}), expected ok and empty`);
      parts.push(`${planned.label} ok and empty (${shape.shape})`);
    } else {
      if (!result.isError) throw new Error(`${step}: ${planned.label} gave the model a result (${shape.shape}), expected input_error`);
      parts.push(`${planned.label} input_error`);
    }
  }
  return parts;
}

/** The answer of a run must hold `needs` and none of `foreign`'s canaries. */
function checkAnswer(step: string, run: RunAnswer, asker: string, needs: string[], foreign: { label: string; canaries: string[] }): void {
  const missing = needs.filter((text) => !run.answer.includes(text));
  if (missing.length > 0) {
    throw new Error(`${step}: run ${run.runId}'s answer lacks tenant ${asker}'s ${missing.join(", ")}${run.removed.length > 0 ? `; the answer guard removed ${run.removed.join(", ")}` : ""}`);
  }
  const leaked = leaksIn(run.answer, foreign.canaries);
  if (leaked.length > 0) throw new Error(`${step}: run ${run.runId}'s answer has tenant ${foreign.label}'s ${leaked.join(", ")}`);
}

/**
 * The check itself (see the header). `platformAdminId` is the platform administrator the API check
 * made (api.ts). Logs one evidence line per step (EVIDENCE_STEPS) and returns them joined; throws
 * with what was awaited or what differed.
 */
export async function checkAssistantTools(deps: ToolsCheckDeps, appHost: string, shape: string, platformAdminId: string): Promise<string> {
  const now = deps.now ?? Date.now;
  const { a, b } = deps.data ?? tenantPair();
  const clashes = canaryClashes(a, b);
  if (clashes.length > 0) throw new Error(`the tenants' canaries do not tell them apart: ${clashes.join("; ")}`);
  const ofA = { label: a.label, canaries: canariesOf(a) };
  const ofB = { label: b.label, canaries: canariesOf(b) };
  const tag = deps.tag ?? newTag;
  const check = new ToolsCheck(deps, appHost);
  const lines: string[] = [];
  const evidence = (step: (typeof EVIDENCE_STEPS)[number], text: string) => {
    const line = `${shape} ${step}: ${text}`;
    deps.log(`evidence: ${line}`);
    lines.push(line);
  };

  // The data.
  const admin: TestSession = { ...newSession(), userId: uuid(platformAdminId) };
  await deps.psql(adminSessionSql(admin));
  const tenantA = await check.tenant(admin, `nightly tools A ${shape}`);
  const tenantB = await check.tenant(admin, `nightly tools B ${shape}`);
  const memberA: Member = { session: newSession(), tenantId: tenantA, data: a };
  const memberB: Member = { session: newSession(), tenantId: tenantB, data: b };
  await deps.psql(memberSessionSql(memberA.session, tenantA, "editor"));
  await deps.psql(memberSessionSql(memberB.session, tenantB, "editor"));
  deps.log(`made tenants A ${tenantA} and B ${tenantB}, each with an editor (A ${memberA.session.userId}, B ${memberB.session.userId})`);
  const times = readingTimes(now());
  for (const member of [memberA, memberB]) {
    await deps.clickhouse(telemetryRowsSql(member.tenantId, member.data, `nightly-${hex(8)}`, times.firstAt, new Date(now())));
  }
  deps.log(`wrote 3 readings each for device ${a.device} of tenant A and device ${b.device} of tenant B into trustiot.telemetry`);
  for (const member of [memberA, memberB]) await check.deviceModel(member);
  const listedA = await check.modelNames(memberA);
  const listedB = await check.modelNames(memberB);
  if (!listedA.includes(a.modelName) || listedA.includes(b.modelName) || !listedB.includes(b.modelName) || listedB.includes(a.modelName)) {
    throw new Error(`the device model lists are not each tenant's own: A lists ${listedA.length} model(s), B ${listedB.length}; A has its own: ${listedA.includes(a.modelName)}, B's: ${listedA.includes(b.modelName)}; B has its own: ${listedB.includes(b.modelName)}, A's: ${listedB.includes(a.modelName)}`);
  }
  const ownA = await check.latest(memberA, a.device);
  const ownB = await check.latest(memberB, b.device);
  if (!ownA.body.includes(a.text) || !ownB.body.includes(b.text)) {
    throw new Error(`each editor must read its own text reading through the ingress: A read it: ${ownA.body.includes(a.text)}, B read it: ${ownB.body.includes(b.text)}`);
  }
  const crossed = await check.latest(memberA, b.device);
  if (crossed.points !== 0 || leaksIn(crossed.body, ofB.canaries).length > 0) throw new Error(`tenant A's editor read ${crossed.points} point(s) of tenant B's device through the ingress, expected none`);
  evidence(
    "setup",
    `tenants A and B with an editor each, device models ${a.modelName} (A) and ${b.modelName} (B), readings ${a.text}, ${a.first}, ${a.last} (A) and ${b.text}, ${b.first}, ${b.last} (B); ` +
      `through the ingress each editor lists only its own model and reads its own readings (A after ${Math.round(ownA.waitedMs / 1000)} s, B after ${Math.round(ownB.waitedMs / 1000)} s), and A reads 0 points of B's device`,
  );

  // Check 1: A's own data.
  const tag1 = tag();
  const plan1 = ownPlan(a, times.from, times.to);
  const run1 = await check.ask(memberA, directive(tag1, plan1.map((p) => p.call)));
  const record1 = (await check.toolRecord(tag1, run1.runId, memberA)).value;
  const parts1 = checkPlan("check 1", plan1, tag1, record1, a.label, ofB);
  checkAnswer("check 1", run1, a.label, [a.modelName, a.text], ofB);
  evidence(
    "check 1",
    `tenant A's editor asked about A's device; run ${run1.runId} answered after ${Math.round(run1.waitedMs / 1000)} s; ${parts1.join(", ")}; ` +
      `each result has A's data and none of B's ${ofB.canaries.length} canaries; the answer has A's model ${a.modelName} and reading ${a.text} and none of B's canaries`,
  );

  // The control: B asks check 2's directive on its own device.
  const tagC = tag();
  const planC = crossPlan(b, b, times.from, times.to);
  const runC = await check.ask(memberB, directive(tagC, planC.map((p) => p.call)));
  const recordC = (await check.toolRecord(tagC, runC.runId, memberB)).value;
  const partsC = checkPlan("control", planC, tagC, recordC, b.label, ofA);
  checkAnswer("control", runC, b.label, [b.text, String(b.last)], ofA);
  evidence(
    "control",
    `tenant B's editor asked check 2's calls about B's own device; run ${runC.runId} answered after ${Math.round(runC.waitedMs / 1000)} s; ${partsC.join(", ")}; ` +
      `the results the model got have B's data and none of A's ${ofA.canaries.length} canaries; the answer has B's reading ${b.text} and ${b.last} and none of A's canaries`,
  );

  // Check 2: B's device, as A.
  const tag2 = tag();
  const plan2 = crossPlan(a, b, times.from, times.to);
  const run2 = await check.ask(memberA, directive(tag2, plan2.map((p) => p.call)));
  const record2 = (await check.toolRecord(tag2, run2.runId, memberA)).value;
  const parts2 = checkPlan("check 2", plan2, tag2, record2, a.label, ofB);
  checkAnswer("check 2", run2, a.label, [], ofB);
  evidence(
    "check 2",
    `tenant A's editor asked the same calls about tenant B's device; run ${run2.runId} answered after ${Math.round(run2.waitedMs / 1000)} s; ${parts2.join(", ")}; ` +
      `none of B's ${ofB.canaries.length} canaries in the answer or in the ${record2.record.results.length} result(s) the model got`,
  );

  // Check 3: the services' logs of the three runs.
  const runs = [
    { name: "run 1", id: run1.runId, calls: record1.calls, tenant: tenantA, label: a.label },
    { name: "the control run", id: runC.runId, calls: recordC.calls, tenant: tenantB, label: b.label },
    { name: "run 2", id: run2.runId, calls: record2.calls, tenant: tenantA, label: a.label },
  ];
  const ids = runs.map((r) => r.id);
  const logged = await pollUntil<string[]>(
    `the assistant's "${RUN_STARTED}" and the identity service's "${DELEGATION_ISSUED}" and "${DELEGATION_EXCHANGED}" lines of runs ${ids.join(", ")}`,
    deps.limits?.logMs ?? LOG_LIMIT_MS,
    deps.limits?.pollMs ?? POLL_MS,
    deps,
    async () => {
      const events = await check.serviceEvents(ids);
      const assistant = events.filter((e) => e.service === "assistant");
      const identity = events.filter((e) => e.service === "identity");
      const seen: string[] = [];
      const counts: string[] = [];
      for (const { name, id, calls, tenant, label } of runs) {
        const started = assistant.filter((e) => e.message === RUN_STARTED && e.fields.run_id === id);
        if (started.some((e) => e.fields.tenant_id !== tenant)) return { fail: `${name} (${id}) started for another tenant than ${label}` };
        const issued = identity.filter((e) => e.message === DELEGATION_ISSUED && e.fields.run === id && e.fields.holder === HOLDER);
        const exchanged = identity.filter((e) => e.message === DELEGATION_EXCHANGED && e.fields.run === id);
        if (exchanged.some((e) => e.fields.holder !== HOLDER)) return { fail: `${name} (${id}) was exchanged by another holder than ${HOLDER}` };
        const byAudience = new Map<string, number>();
        for (const e of exchanged) byAudience.set(String(e.fields.audience), (byAudience.get(String(e.fields.audience)) ?? 0) + 1);
        // Every call that reached its service with an answer it accepted (ok) or refused as the
        // user (denied) exchanged a token for it first; a refused input may have, too.
        const needed = new Map<string, number>();
        for (const call of calls) {
          if (call.outcome !== "ok" && call.outcome !== "denied") continue;
          const audience = AUDIENCE_OF[call.tool] ?? "unknown";
          needed.set(audience, (needed.get(audience) ?? 0) + 1);
        }
        const audiences = [...byAudience].map(([k, n]) => `${k} ×${n}`).join(" and ") || "none";
        const short = [...needed].filter(([audience, n]) => (byAudience.get(audience) ?? 0) < n);
        const extra = [...byAudience.keys()].filter((audience) => !needed.has(audience));
        seen.push(`${name}: ${started.length} started, ${issued.length} issued, exchanged ${audiences}`);
        if (started.length !== 1 || issued.length !== 1 || short.length > 0) return { pending: seen.join("; ") };
        if (extra.length > 0) return { fail: `${name} (${id}) was exchanged for ${extra.join(", ")}, which none of its tools calls` };
        counts.push(`${name}: ${RUN_STARTED} for tenant ${label}, ${calls.length} × ${TOOL_CALL_COMPLETED}; identity: ${DELEGATION_ISSUED} to ${HOLDER}, ${DELEGATION_EXCHANGED} for ${audiences}`);
      }
      return { done: counts };
    },
  );
  evidence("check 3", `the logs match runs ${ids.join(", ")}: ${logged.value.join("; ")}; every model call of each run carried its asker's tenant in ai-gateway-tenant`);
  return lines.join("; ");
}

/** The two checks of the smoke (smoke.ts) that toolsCheckProblems looks for, by name. */
export const STAND_IN_CHECK =
  "helm upgrade with one more values file, which sets only the AI gateway's image, makes the scripted stand-in the AI gateway: the same upgrade run first with --dry-run=server, and then the upgrade itself, change the release only in that image and in the migration Job the chart runs again for a new image, and every ai-gateway pod runs the stand-in (Laysi/trustiot-v2#648)";
export const TOOLS_CHECK =
  "a member's questions make the stand-in call the assistant's tools: they read the member's own tenant's data as that member through the mesh and never another tenant's, in the answer and in every result the model got; the other tenant reads its own data through the same calls; and the logs show the runs and their token exchanges (Laysi/trustiot-v2#648)";

/**
 * Why a shape's checks do not show the assistant's tool check run to its last step, on a release
 * that runs the assistant: both checks above must be there once and have passed, and the tool
 * check's result must hold every evidence step of the shape. Empty when they do, and for a release
 * without the assistant. A guard against a later change that skips the check and still passes.
 */
export function toolsCheckProblems(checks: readonly Check[], objects: K8sObject[], shape: string): string[] {
  const runsAssistant = objects.some((o) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === "assistant");
  if (!runsAssistant) return [];
  const problems: string[] = [];
  for (const [what, name] of [
    ["the stand-in's upgrade", STAND_IN_CHECK],
    ["the tool check", TOOLS_CHECK],
  ] as const) {
    const ran = checks.filter((c) => c.name === name);
    if (ran.length !== 1) problems.push(`${what} ran ${ran.length} time(s), expected once`);
    else if (!ran[0]?.ok) problems.push(`${what} failed`);
  }
  const detail = checks.find((c) => c.name === TOOLS_CHECK && c.ok)?.detail ?? "";
  const missing = EVIDENCE_STEPS.filter((step) => !detail.includes(`${shape} ${step}: `));
  if (missing.length > 0) problems.push(`the tool check's result has no ${missing.join(", ")} of shape ${shape}`);
  return problems;
}

// ---------------------------------------------------------------------------------------------
// The cluster.

/** `--selector` of the pods of a component's Deployment in the release, from its matchLabels. */
export function selectorOf(objects: K8sObject[], component: string): string {
  const deployments = objects.filter((o) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === component);
  if (deployments.length !== 1) throw new Error(`the release should have one Deployment of ${component}, found ${deployments.length}`);
  const labels = asMap(asMap(deployments[0]?.spec?.selector).matchLabels);
  const pairs = Object.entries(labels).map(([key, value]) => `${key}=${String(value)}`);
  if (pairs.length === 0) throw new Error(`Deployment ${deployments[0]?.metadata?.name} has no matchLabels`);
  return pairs.sort().join(",");
}

/** The pods of the release's ai-gateway, for waitForAiGatewayDouble. */
export function aiGatewayPods(ctx: ApiCheckContext, namespace: string, objects: K8sObject[]): () => Promise<unknown> {
  const selector = selectorOf(objects, AI_GATEWAY);
  return async () => {
    const listed = await run(ctx.log, [ctx.kubectl, "get", "pods", "--namespace", namespace, "--selector", selector, "--output", "json"], {
      env: { KUBECONFIG: ctx.kubeconfig },
      timeoutMs: 60_000,
      quiet: true,
    });
    return JSON.parse(listed.stdout) as unknown;
  };
}

/** The container named like its component in that component's Deployments of the release. */
function componentContainers(objects: K8sObject[], component: string): Json[] {
  const deployments = objects.filter((o) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === component);
  return deployments.flatMap((d) => asArray(asMap(asMap(asMap(d.spec).template).spec).containers).filter(isMap)).filter((c) => c.name === component);
}

/** The ports a component's container declares in the release, by name. */
export function containerPortsOf(objects: K8sObject[], component: string): Record<string, number> {
  const ports = componentContainers(objects, component).flatMap((c) => asArray(c.ports).filter(isMap));
  return Object.fromEntries(ports.flatMap((p) => (typeof p.name === "string" && typeof p.containerPort === "number" ? [[p.name, p.containerPort]] : [])));
}

/** The tools the release's assistant registers, from the callee URLs the chart gives it (TOOLS_OF_URL). */
export function registeredTools(objects: K8sObject[]): string[] {
  const containers = componentContainers(objects, "assistant");
  if (containers.length !== 1) throw new Error(`the release should have one assistant container, found ${containers.length}`);
  const names = new Set(asArray(containers[0]?.env).filter(isMap).map((e) => e.name));
  return Object.entries(TOOLS_OF_URL).flatMap(([name, tools]) => (names.has(name) ? [...tools] : []));
}

/**
 * The bundled ClickHouse's client as its administrator, run with `bash -c` in its pod. The
 * password goes from the file the pod mounts into the client's environment (CLICKHOUSE_PASSWORD,
 * which clickhouse-client reads), never into an argument that a process listing shows; a missing
 * file ends the script before the client starts. The statement comes on stdin.
 */
export const CLICKHOUSE_ADMIN = 'CLICKHOUSE_PASSWORD="$(cat "$CLICKHOUSE_PASSWORD_FILE")" || exit 1; export CLICKHOUSE_PASSWORD; exec clickhouse-client --user "$CLICKHOUSE_USER"';

/**
 * Runs checkAssistantTools against the release in `namespace`, after the stand-in replaced the
 * gateway: requests through `kubectl port-forward` to the ingress gateway's Service (`ca`: the
 * certificate the smoke made for an HTTPS listener), psql in the CloudNativePG primary, the
 * readings' INSERT and the services' events in the bundled ClickHouse's pod, and the stand-in's
 * lines through a `kubectl port-forward` to the loopback lines port of each ai-gateway pod
 * (STAND_IN_LINES_PORT). Statements, answers and lines stay out of the run log. Fails before any
 * request when the release's assistant does not register every tool the check calls.
 */
export async function runAssistantToolsCheck(
  ctx: ApiCheckContext,
  namespace: string,
  objects: K8sObject[],
  ca: string | undefined,
  platformAdminId: string,
  shape: string,
): Promise<string> {
  const env = { KUBECONFIG: ctx.kubeconfig };
  const unregistered = ALL_TOOLS.filter((tool) => !registeredTools(objects).includes(tool));
  if (unregistered.length > 0) {
    throw new Error(`the release's assistant registers no ${unregistered.join(", ")}: the chart gives it a callee URL for each tool's service (${Object.keys(TOOLS_OF_URL).join(", ")}) only when mesh.calls.assistant lists it, and the check needs every tool`);
  }
  const { appHost, psql, gateway } = await ingressAccess(ctx, namespace, objects, ca);
  const pod = clickhousePodOf(objects);
  const clickhouse = async (sql: string) =>
    (
      await run(ctx.log, [ctx.kubectl, "exec", "--namespace", namespace, "-i", pod, "-c", "clickhouse", "--", "bash", "-c", CLICKHOUSE_ADMIN], {
        env,
        stdin: sql,
        timeoutMs: 60_000,
        quiet: true,
      })
    ).stdout;
  // The stand-in's pods as waitForAiGatewayDouble left them: each keeps the lines of the questions it answered.
  const pods = componentPods(await aiGatewayPods(ctx, namespace, objects)(), AI_GATEWAY).filter((p) => p.image === AI_GATEWAY_DOUBLE_IMAGE && p.ready && !p.terminating);
  if (pods.length === 0) throw new Error(`no ready ${AI_GATEWAY} pod runs ${AI_GATEWAY_DOUBLE_IMAGE}`);
  const standIns = pods.map((p) => new PortForward(ctx, namespace, `pod/${p.name}`, STAND_IN_LINES_PORT, undefined));
  const standInLines = async (tag: string): Promise<unknown[]> => {
    const lines: unknown[] = [];
    for (const [index, standIn] of standIns.entries()) {
      const target = `${STAND_IN_LINES_PATH}?tag=${tag}`;
      const response = await standIn.send({ target, hostHeader: "localhost" });
      if (response.status !== 200) throw new Error(`GET ${STAND_IN_LINES_PATH} of pod ${pods[index]?.name} answered ${response.status}, expected 200`);
      lines.push(...asArray(parsedJson(`GET ${STAND_IN_LINES_PATH}`, response.status, response.body).lines));
    }
    return lines;
  };
  const opened: PortForward[] = [];
  try {
    for (const tunnel of [gateway, ...standIns]) {
      await tunnel.open();
      opened.push(tunnel);
    }
    return await checkAssistantTools({ send: (request) => gateway.send(request), psql, clickhouse, standInLines, log: (line) => ctx.log.line(line) }, appHost, shape, platformAdminId);
  } finally {
    for (const tunnel of opened) tunnel.close();
  }
}
