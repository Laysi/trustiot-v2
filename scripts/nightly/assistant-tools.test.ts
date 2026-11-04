// The assistant's tool check (assistant-tools.ts, Laysi/trustiot-v2#648) without a cluster. The
// fake release answers the ingress requests the way identity, assets, telemetry-query and the
// assistant do, and its assistant asks the real stand-in (ai-gateway-double/server.ts, run as a
// child process) with requests shaped like services/assistant/src/model/anthropic.rs, runs the
// tools on the fake's data and logs what the real services log, as the log table holds it. The
// check reads the stand-in's lines from its lines port, as in the cluster. So the directives the
// check writes, the stand-in's script and the lines and events the check reads are tested
// together; what the real services answer is the nightly's to prove.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { type AiGatewayDouble, startAiGatewayDouble } from "../../deploy/chart/tests/e2e/ai-gateway-double-process.ts";
import { ConnectionError, type RawRequest, type RawResponse } from "../../deploy/chart/tests/e2e/raw-client.ts";
import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut } from "../../deploy/chart/tests/timing.ts";
import { apiValues, ASSISTANT_CONVERSATIONS_PATH, routesOfValues, SESSION_COOKIE, TENANTS_PATH } from "./api.ts";
import {
  AI_GATEWAY,
  aiGatewayDoubleValues,
  ALL_TOOLS,
  AUDIENCE_OF,
  BASE_TABLE_SQL,
  canariesOf,
  canaryClashes,
  CLICKHOUSE_ADMIN,
  checkAssistantTools,
  componentPods,
  containerPortsOf,
  crossPlan,
  DEVICE_MODELS_PATH,
  directive,
  dryRunManifest,
  EVIDENCE_STEPS,
  GATEWAY_ID,
  LOG_TABLE,
  parseServiceEvents,
  MODEL_TOOL,
  newTenantData,
  NUMBER_POINT,
  ownPlan,
  PREFIX_SQL,
  readingTimes,
  registeredTools,
  releaseChange,
  releaseChangeProblems,
  resultShape,
  selectorOf,
  SERVICE_IMAGES_ANNOTATION,
  STAND_IN_CHECK,
  STAND_IN_LINES_PATH,
  STAND_IN_LINES_PORT,
  standInEvents,
  standInRecord,
  serviceEventsSql,
  TELEMETRY_TOOLS,
  telemetryCalls,
  telemetryRowsSql,
  type TenantData,
  tenantPair,
  TEXT_POINT,
  TOOLS_CHECK,
  toolsCheckProblems,
  type ToolsCheckDeps,
  waitForAiGatewayDouble,
} from "./assistant-tools.ts";
import { pinnedTool } from "./exec.ts";
import { type K8sObject, parseManifests } from "./manifests.ts";
import { AI_GATEWAY_DOUBLE_IMAGE } from "./platform.ts";
import type { Check } from "./report.ts";
import { discoverShapes, valuesArgs } from "./smoke.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");
const APP = "app.saas.trustiot.test";
/** 2026-11-04T10:00:30Z: the fake clock's start. */
const START = Date.UTC(2026, 10, 4, 10, 0, 30);

type Json = Record<string, unknown>;

let double: AiGatewayDouble;

beforeAll(async () => {
  double = await startAiGatewayDouble();
});

afterAll(() => {
  double?.stop();
});

/** Fixed tenant data: A's and B's canaries are told apart. */
function fixedData(): { a: TenantData; b: TenantData } {
  return {
    a: { label: "A", device: crypto.randomUUID(), modelName: "nightly-m-0123456789abcdef", text: "nightly-t-0123456789abcdef", first: 123456.625, last: 123456.625 + 2001.25 },
    b: { label: "B", device: crypto.randomUUID(), modelName: "nightly-m-fedcba9876543210", text: "nightly-t-fedcba9876543210", first: 654321.625, last: 654321.625 + 8003.25 },
  };
}

interface ToolAnswer {
  outcome: string;
  isError: boolean;
  text: string;
}

interface WorldOptions {
  /** The tools the fake assistant offers the model (all five unless given). */
  offered?: string[];
  /** Replaces what a tool returns on a device of another tenant than the caller's. */
  foreign?: (tool: string, data: { text: string; last: number }) => ToolAnswer | undefined;
  /** Replaces what a tool returns on the caller's own device. */
  own?: (tool: string, device: string) => ToolAnswer | undefined;
  /** The tenant (by its data's label) whose SQL without a device filter also returns the other tenant's text reading. */
  sqlCrossesFor?: string;
  /** The tenant (by its data's label) whose SQL on the base table runs and returns its own text reading. */
  baseTableRunsFor?: string;
  /** How every run ends instead of answered. */
  runEnds?: { status: string; result: string; message_key: string };
  /** An audience whose exchanges the identity service does not log. */
  dropExchange?: string;
  /** The ai-gateway-tenant header the fake assistant sends instead of the caller's tenant. */
  tenantHeader?: string;
  /** What the answer guard does to an answer. */
  guard?: (answer: string) => { text: string; removed: string[] };
  /** The first question POST is answered 503 question.busy. */
  busyOnce?: boolean;
  /** The first question POST starts the run but its answer is lost. */
  lostQuestionOnce?: boolean;
}

/** A row of the log table as serviceEventsSql selects it: the collector stores every field as text. */
const serviceEvent = (service: string, message: string, fields: Json) => ({ service, message, fields: Object.fromEntries(Object.entries(fields).map(([key, value]) => [key, String(value)])) });

/** The stand-in's lines of a tag from its lines port, as runAssistantToolsCheck reads them. */
async function standInLinesOf(tag: string): Promise<unknown[]> {
  const response = await double.send({ target: `${STAND_IN_LINES_PATH}?tag=${tag}`, hostHeader: "localhost" }, double.linesPort);
  if (response.status !== 200) throw new Error(`the stand-in's lines answered ${response.status}`);
  return (JSON.parse(response.body) as { lines: unknown[] }).lines;
}

const errorBody = (code: string, key: string) => JSON.stringify({ code, message: "refused", message_key: key });
const respond = (status: number, body: unknown): RawResponse => ({ status, headers: {}, body: typeof body === "string" ? body : JSON.stringify(body), ms: 1 });

/** The fake release (see the file's header). */
function world(options: WorldOptions = {}) {
  let clock = START;
  const sessions = new Map<string, string | null>();
  const models = new Map<string, string[]>();
  const readings = new Map<string, { tenant: string; numbers: number[]; text: string }>();
  const waitingAssets = new Set<string>();
  const waitingTelemetry = new Set<string>();
  const conversations = new Map<string, { tenant: string; question?: string; run?: Json; answer?: { text: string; removed: string[] }; polls: number }>();
  const events: ReturnType<typeof serviceEvent>[] = [];
  const logged: string[] = [];
  const statements: string[] = [];
  let busy = options.busyOnce === true;
  let lose = options.lostQuestionOnce === true;

  const tenantOf = (request: RawRequest): string | null | undefined => {
    const cookie = request.headers?.find(([name]) => name === "cookie")?.[1];
    const value = cookie?.startsWith(`${SESSION_COOKIE}=`) ? cookie.slice(SESSION_COOKIE.length + 1) : undefined;
    if (value === undefined) return undefined;
    return sessions.get(new Bun.CryptoHasher("sha256").update(Buffer.from(value, "base64url")).digest("hex"));
  };

  /** The tenant ID of the data labelled `label`, from its readings. */
  const tenantLabelled = (label: string | undefined): string | undefined => {
    const data = [deps.data?.a, deps.data?.b].find((d) => d?.label === label);
    return data === undefined ? undefined : readings.get(data.device)?.tenant;
  };
  const sqlRows = (rows: unknown[][]): ToolAnswer => ({
    outcome: "ok",
    isError: false,
    text: JSON.stringify({ columns: [{ name: "point", type: "String" }, { name: "value_string", type: "String" }], rows, row_count: rows.length, queryable_from: {}, queried_at: "2026-11-04T10:00:00Z" }),
  });

  function toolAnswer(tool: string, input: Json, tenant: string): ToolAnswer {
    const sql = String(input.sql ?? "");
    const textRows = (all: boolean) => [...readings.values()].filter((r) => all || r.tenant === tenant).map((r) => [TEXT_POINT, r.text]);
    if (tool === "telemetry_sql" && sql.includes("FROM trustiot.telemetry")) {
      // telemetry-query's SQL check refuses the table (422), which the tool reports as input_error.
      if (tenantLabelled(options.baseTableRunsFor) === tenant) return sqlRows(textRows(false));
      return { outcome: "input_error", isError: true, text: "Only the telemetry views can be read; trustiot.telemetry is not one of them." };
    }
    if (tool === "telemetry_sql" && sql.includes("startsWith(value_string")) return sqlRows(textRows(tenantLabelled(options.sqlCrossesFor) === tenant));
    const series = Array.isArray(input.series) ? (input.series as Json[]) : [];
    const device = (input.device_id ?? series[0]?.device_id ?? /'([0-9a-f-]{36})'/.exec(sql)?.[1]) as string | undefined;
    const found = device === undefined ? undefined : readings.get(device);
    const own = found !== undefined && found.tenant === tenant;
    const replaced = own ? options.own?.(tool, device ?? "") : found === undefined ? undefined : options.foreign?.(tool, { text: found.text, last: found.numbers[1] ?? 0 });
    if (replaced !== undefined) return replaced;
    const [first, last] = own ? (found.numbers as [number, number]) : [0, 0];
    const text = own ? found.text : "";
    let result: Json;
    switch (tool) {
      case MODEL_TOOL: {
        const rows = (models.get(tenant) ?? []).map((name) => [crypto.randomUUID(), name, 1, 0, "2026-11-04T10:00:00Z"]);
        result = { columns: ["model_id", "name", "revision", "point_count", "updated_at"], rows, total: rows.length };
        break;
      }
      case "telemetry_latest": {
        const rows = own ? [[NUMBER_POINT, "2026-11-04T09:58:00Z", last], [TEXT_POINT, "2026-11-04T09:58:00Z", text]] : [];
        result = { device_id: device, earliest: "2026-10-28T10:00:00Z", columns: ["point", "time", "value"], rows, total: rows.length };
        break;
      }
      case "telemetry_statistics":
        result = {
          range: { from: input.from, to: input.to, tz: "UTC" },
          series: series.map((s) =>
            !own
              ? { device_id: s.device_id, point: s.point, status: "no_data", count: 0, min: null, max: null, first: null, last: null, rows: [] }
              : s.point === NUMBER_POINT
                ? { device_id: s.device_id, point: s.point, status: "ok", count: 2, min: first, max: last, avg: (first + last) / 2, first: { time: "t", value: first }, last: { time: "t", value: last } }
                : { device_id: s.device_id, point: s.point, status: "ok", count: 1, min: null, max: null, first: { time: "t", value: text }, last: { time: "t", value: text } },
          ),
        };
        break;
      case "telemetry_usage":
        result = { range: {}, series: series.map((s) => (own ? { point: s.point, status: "ok", total: last - first, readings: 2 } : { point: s.point, status: "no_data", total: null, readings: 0 })) };
        break;
      case "telemetry_sql": {
        const rows = own ? [[NUMBER_POINT, "float", first, ""], [NUMBER_POINT, "float", last, ""], [TEXT_POINT, "string", 0, text]] : [];
        result = { columns: [{ name: "point", type: "String" }], rows, row_count: rows.length, queryable_from: {}, queried_at: "2026-11-04T10:00:00Z" };
        break;
      }
      default:
        return { outcome: "unknown_tool", isError: true, text: "no such tool" };
    }
    return { outcome: "ok", isError: false, text: JSON.stringify(result) };
  }

  /** The fake assistant's question run: two model calls to the real stand-in and the tools between them. */
  async function runQuestion(conversation: string, tenant: string, question: string): Promise<string> {
    const run = crypto.randomUUID();
    events.push(serviceEvent("assistant", "question run started", { run_id: run, tenant_id: tenant, lifetime_secs: 900, deadline_in_secs: 600 }));
    events.push(serviceEvent("identity", "issued a run delegation", { holder: "assistant", run, lifetime_secs: 900 }));
    const tools = (options.offered ?? ALL_TOOLS).map((name) => ({ name, description: `The ${name} tool.`, input_schema: { type: "object" } }));
    const ask = async (messages: Json[]) => {
      const response = await double.send({
        target: "/internal/roles/smoke/v1/messages",
        method: "POST",
        hostHeader: "trustiot-ai-gateway:8080",
        headers: [
          ["content-type", "application/json"],
          ["ai-gateway-tenant", options.tenantHeader ?? tenant],
        ],
        body: JSON.stringify({ max_tokens: 16000, system: [{ type: "text", text: "You are the TrustIoT assistant." }], tools, messages }),
      });
      if (response.status !== 200) throw new Error(`the stand-in answered ${response.status}: ${response.body}`);
      return JSON.parse(response.body) as { stop_reason: string; content: Json[] };
    };
    const asked = { role: "user", content: question };
    const first = await ask([asked]);
    let answer = String(first.content[0]?.text ?? "");
    if (first.stop_reason === "tool_use") {
      const results = first.content.map((use) => {
        const name = String(use.name);
        const got = toolAnswer(name, use.input as Json, tenant);
        if ((got.outcome === "ok" || got.outcome === "denied") && AUDIENCE_OF[name] !== options.dropExchange) {
          events.push(serviceEvent("identity", "exchanged a run delegation", { holder: "assistant", audience: AUDIENCE_OF[name], run }));
        }
        events.push(
          serviceEvent("assistant", "tool call completed", { run_id: run, conversation_id: conversation, tenant_id: tenant, tool: name, outcome: got.outcome, duration_ms: 3, result_bytes: got.text.length }),
        );
        return { type: "tool_result", tool_use_id: use.id, content: [{ type: "text", text: got.text }], ...(got.isError ? { is_error: true } : {}) };
      });
      const second = await ask([asked, { role: "assistant", content: first.content }, { role: "user", content: results }]);
      answer = String(second.content[0]?.text ?? "");
    }
    const entry = conversations.get(conversation);
    if (entry === undefined) throw new Error("no such conversation");
    entry.question = question;
    if (options.runEnds !== undefined) entry.run = { id: run, ...options.runEnds };
    else {
      entry.run = { id: run, status: "answered", result: null, message_key: null };
      entry.answer = options.guard?.(answer) ?? { text: answer, removed: [] };
    }
    return run;
  }

  async function send(request: RawRequest): Promise<RawResponse> {
    if (request.hostHeader !== APP) return respond(404, "");
    const tenant = tenantOf(request);
    if (tenant === undefined) return respond(401, errorBody("unauthenticated", "auth.unauthenticated"));
    const body = request.body === undefined ? {} : (JSON.parse(request.body) as Json);
    const target = request.target;
    const method = request.method ?? "GET";
    if (method === "POST" && target === TENANTS_PATH) {
      if (tenant !== null) return respond(403, errorBody("permission_denied", "auth.permission_denied"));
      const id = crypto.randomUUID();
      waitingAssets.add(id);
      waitingTelemetry.add(id);
      return respond(201, { tenant_id: id, name: body.name });
    }
    if (tenant === null) return respond(403, errorBody("permission_denied", "auth.permission_denied"));
    if (target === DEVICE_MODELS_PATH && method === "POST") {
      if (waitingAssets.delete(tenant)) return respond(503, errorBody("unavailable", "device_model.tenant_not_ready"));
      models.set(tenant, [...(models.get(tenant) ?? []), String(body.name)]);
      return respond(201, { name: body.name });
    }
    if (target === DEVICE_MODELS_PATH) return respond(200, { models: (models.get(tenant) ?? []).map((name) => ({ name })) });
    const latest = /^\/api\/telemetry\/v1\/devices\/([0-9a-f-]{36})\/latest$/.exec(target);
    if (latest !== null) {
      if (waitingTelemetry.delete(tenant)) return respond(404, errorBody("not_found", "telemetry.account_not_ready"));
      const found = readings.get(latest[1] ?? "");
      const points = found?.tenant === tenant ? [{ point: TEXT_POINT, value: found.text }, { point: NUMBER_POINT, value: found.numbers[1] }] : [];
      return respond(200, { device_id: latest[1], earliest: null, points });
    }
    if (method === "POST" && target === ASSISTANT_CONVERSATIONS_PATH) {
      const id = crypto.randomUUID();
      conversations.set(id, { tenant, polls: 0 });
      return respond(201, { id });
    }
    const asked = /^\/api\/assistant\/v1\/conversations\/([0-9a-f-]{36})\/questions$/.exec(target);
    if (asked !== null && method === "POST") {
      if (busy) {
        busy = false;
        return respond(503, errorBody("unavailable", "question.busy"));
      }
      const conversation = asked[1] ?? "";
      if (conversations.get(conversation)?.tenant !== tenant) return respond(404, errorBody("not_found", "conversation.not_found"));
      const run = await runQuestion(conversation, tenant, String(body.question));
      if (lose) {
        lose = false;
        throw new ConnectionError("the connection closed before the response head");
      }
      return respond(202, { id: run, status: "running" });
    }
    const shown = /^\/api\/assistant\/v1\/conversations\/([0-9a-f-]{36})$/.exec(target);
    if (shown !== null) {
      const entry = conversations.get(shown[1] ?? "");
      if (entry?.tenant !== tenant) return respond(404, errorBody("not_found", "conversation.not_found"));
      entry.polls += 1;
      const running = entry.run !== undefined && entry.polls === 1;
      const messages = [
        ...(entry.question === undefined ? [] : [{ kind: "question", text: entry.question }]),
        ...(entry.answer === undefined || running ? [] : [{ kind: "answer", text: entry.answer.text, ...(entry.answer.removed.length > 0 ? { removed: entry.answer.removed } : {}) }]),
      ];
      return respond(200, { id: shown[1], messages, latest_run: entry.run === undefined ? null : running ? { ...entry.run, status: "running" } : entry.run });
    }
    return respond(404, errorBody("not_found", "route.not_found"));
  }

  const deps: ToolsCheckDeps = {
    send,
    psql: async (sql) => {
      const hash = /'\\x([0-9a-f]{64})'/.exec(sql)?.[1];
      if (hash === undefined) throw new Error("the fake reads session SQL only");
      const membership = /INSERT INTO identity\.memberships \(tenant_id, user_id, tenant_role\) VALUES \('([0-9a-f-]{36})', '[0-9a-f-]{36}', '(\w+)'\)/.exec(sql);
      if (membership !== null && membership[2] !== "editor") throw new Error(`the check makes editors, not ${membership[2]}`);
      sessions.set(hash, membership?.[1] ?? null);
      return "";
    },
    clickhouse: async (sql) => {
      if (sql.startsWith("SELECT")) {
        expect(sql).toContain(`FROM ${LOG_TABLE}`);
        const runs = new Set([...sql.matchAll(/'([0-9a-f-]{36})'/g)].map((m) => m[1]));
        return events
          .filter((e) => runs.has(e.fields.run_id ?? e.fields.run))
          .map((e) => JSON.stringify(e))
          .join("\n");
      }
      statements.push(sql);
      const row = /\('([0-9a-f-]{36})', '([^']+)', '(nightly-[0-9a-f]{16})', 1, '([0-9a-f-]{36})', '(\w+)', '([^']+)', '([^']+)', '(float|string)', ([0-9.]+), '([^']*)'\)/g;
      for (const m of sql.matchAll(row)) {
        const [, tenant = "", gateway, , device = "", point, , , , float = "0", text = ""] = m;
        expect(gateway).toBe(GATEWAY_ID);
        const entry = readings.get(device) ?? { tenant, numbers: [], text: "" };
        if (point === NUMBER_POINT) entry.numbers.push(Number(float));
        else entry.text = text;
        readings.set(device, entry);
      }
      return "";
    },
    standInLines: standInLinesOf,
    log: (line) => logged.push(line),
    sleep: async (ms) => {
      clock += ms;
    },
    now: () => clock,
    data: fixedData(),
  };
  return { deps, logged, statements, readings };
}

/** The containers of a Deployment's pod template. */
function asContainers(o: K8sObject | undefined): { name: string; env?: { name: string; value?: string }[] }[] {
  return ((o?.spec?.template as { spec?: { containers?: { name: string; env?: { name: string; value?: string }[] }[] } } | undefined)?.spec?.containers ?? []);
}

const evidenceOf = (logged: string[]) => logged.filter((line) => line.startsWith("evidence: "));

describe("the check against a fake release and the real stand-in", () => {
  test("A reads its own data through every tool and none of B's, B reads its own through check 2's calls; the logs show the three runs and their exchanges; one evidence line per step", async () => {
    const { deps, logged, statements } = world({ busyOnce: true });
    const data = deps.data!;
    const detail = await checkAssistantTools(deps, APP, "saas", crypto.randomUUID());
    const evidence = evidenceOf(logged);
    expect(evidence.map((line) => /^evidence: saas ([a-z0-9 ]+): /.exec(line)?.[1])).toEqual([...EVIDENCE_STEPS]);
    expect(evidence[0]).toContain("saas setup: tenants A and B");
    expect(evidence[1]).toMatch(/^evidence: saas check 1: .*device_models ok .*telemetry_latest ok .*telemetry_statistics ok .*telemetry_usage ok .*telemetry_sql ok /);
    expect(evidence[1]).toContain(`the answer has A's model ${data.a.modelName} and reading ${data.a.text}`);
    expect(evidence[2]).toMatch(
      /^evidence: saas control: .*telemetry_latest \(B's device\) ok .*telemetry_statistics \(B's device\) ok .*telemetry_usage \(B's device\) ok .*telemetry_sql \(B's device\) ok .*telemetry_sql \(no device filter\) ok .*telemetry_sql \(trustiot\.telemetry\) input_error/,
    );
    expect(evidence[2]).toContain(`the answer has B's reading ${data.b.text} and ${data.b.last}`);
    expect(evidence[3]).toMatch(
      /^evidence: saas check 2: .*telemetry_latest \(B's device\) ok and empty .*telemetry_statistics \(B's device\) ok and empty .*telemetry_usage \(B's device\) ok and empty .*telemetry_sql \(B's device\) ok and empty .*telemetry_sql \(no device filter\) ok \(.* with 1 row\(s\).*telemetry_sql \(trustiot\.telemetry\) input_error/,
    );
    expect(evidence[4]).toMatch(
      /^evidence: saas check 3: .*run 1: question run started for tenant A, 5 × tool call completed; identity: issued a run delegation to assistant, exchanged a run delegation for assets ×1 and telemetry-query ×4; the control run: question run started for tenant B, 6 × tool call completed;.*telemetry-query ×5; run 2: question run started for tenant A, 6 × tool call completed;.*telemetry-query ×5/,
    );
    expect(detail.split("; saas ").length).toBe(EVIDENCE_STEPS.length);
    // No evidence line or log line names a cookie.
    for (const line of logged) expect(line).not.toMatch(/__Host-|cookie=/);
    // Two INSERTs, one per tenant, each with its three readings.
    expect(statements).toHaveLength(2);
    for (const sql of statements) expect(sql.match(/\('/g)).toHaveLength(3);
  });

  test("a lost answer to the question POST adopts the run the conversation shows", async () => {
    const { deps, logged } = world({ lostQuestionOnce: true });
    await checkAssistantTools(deps, APP, "onprem", crypto.randomUUID());
    expect(evidenceOf(logged)).toHaveLength(EVIDENCE_STEPS.length);
  });

  test("a tool the smoke's assistant does not register fails check 1, naming the tool", async () => {
    const { deps, logged } = world({ offered: [MODEL_TOOL, "telemetry_latest", "telemetry_statistics", "telemetry_usage"] });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow("check 1: the smoke's assistant does not offer telemetry_sql to the model");
    expect(evidenceOf(logged)).toHaveLength(1);
  });

  test("one denied call on B's device fails check 2: telemetry-query answers another tenant's device with 200 and no data", async () => {
    const { deps } = world({ foreign: (tool) => (tool === "telemetry_latest" ? { outcome: "denied", isError: true, text: "You may not read this." } : undefined) });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow("check 2: the assistant logged telemetry_latest denied, expected ok");
  });

  test("a run 2 whose calls on B's device are all denied fails check 2, though nothing of B leaked", async () => {
    const { deps, logged } = world({ foreign: () => ({ outcome: "denied", isError: true, text: "You may not read this." }) });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(/^check 2: the assistant logged telemetry_latest denied, expected ok$/);
    // The control passed: B's own calls are answered, so only A's token path is at fault.
    expect(evidenceOf(logged).map((line) => /^evidence: saas ([a-z0-9 ]+): /.exec(line)?.[1])).toEqual(["setup", "check 1", "control"]);
  });

  test("B's reading in a result the model got fails check 2 and names the call", async () => {
    const { deps } = world({ foreign: (tool, b) => (tool === "telemetry_usage" ? { outcome: "ok", isError: false, text: JSON.stringify({ series: [{ status: "ok", total: b.last }] }) } : undefined) });
    const b = deps.data!.b;
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(`check 2: telemetry_usage (B's device) handed the model tenant B's ${b.last}`);
  });

  test("a call on B's device that ends input_error fails check 2 with its outcome", async () => {
    const { deps } = world({ foreign: (tool) => (tool === "telemetry_statistics" ? { outcome: "input_error", isError: true, text: "The range is not valid." } : undefined) });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow("check 2: the assistant logged telemetry_statistics input_error, expected ok");
  });

  test("a result on B's device that is ok but not empty fails check 2 with its shape", async () => {
    const { deps } = world({ foreign: (tool) => (tool === "telemetry_latest" ? { outcome: "ok", isError: false, text: JSON.stringify({ rows: [["p", "t", 1]], total: 1 }) } : undefined) });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(
      "check 2: telemetry_latest (B's device) gave the model a result that is not empty ({rows,total} with 1 row(s), total 1), expected ok and empty",
    );
  });

  test("SQL without a device filter that returns B's text reading to A fails check 2", async () => {
    const { deps } = world({ sqlCrossesFor: "A" });
    const b = deps.data!.b;
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(`check 2: telemetry_sql (no device filter) handed the model tenant B's ${b.text}`);
  });

  test("SQL on the base table that runs for A fails check 2, even with A's own rows only", async () => {
    const { deps } = world({ baseTableRunsFor: "A" });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow("check 2: the assistant logged telemetry_sql ok, ok, ok, expected input_error, ok, ok");
  });

  test("B's own result without B's data fails the control, before A asks about B's device", async () => {
    const empty = { outcome: "ok", isError: false, text: JSON.stringify({ rows: [], total: 0 }) };
    let bDevice = "";
    const { deps, logged } = world({ own: (tool, device) => (tool === "telemetry_latest" && device === bDevice ? empty : undefined) });
    const b = deps.data!.b;
    bDevice = b.device;
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(`control: telemetry_latest (B's device)'s result ({rows,total} with 0 row(s), total 0) lacks tenant B's ${b.text}, ${b.last}`);
    expect(evidenceOf(logged)).toHaveLength(2);
  });

  test("a result on A's own device without A's data fails check 1", async () => {
    const { deps } = world({ own: (tool) => (tool === "telemetry_sql" ? { outcome: "ok", isError: false, text: JSON.stringify({ columns: [], rows: [], row_count: 0 }) } : undefined) });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow("check 1: telemetry_sql's result ({columns,row_count,rows} with 0 row(s), row_count 0) lacks tenant A's");
  });

  test("a run that does not end answered fails with its status, result and message key", async () => {
    const { deps } = world({ runEnds: { status: "failed", result: "model_error", message_key: "assistant.model_unavailable" } });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(/ended failed \(result model_error, message_key assistant\.model_unavailable\), expected answered/);
  });

  test("an answer the guard cut fails check 1 and says what the guard removed", async () => {
    const { deps } = world({ guard: (answer) => ({ text: answer.replaceAll("nightly-m-", "removed-"), removed: ["sql"] }) });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(/check 1: run [0-9a-f-]{36}'s answer lacks tenant A's nightly-m-0123456789abcdef; the answer guard removed sql/);
  });

  test("a model call with another tenant's header fails while the logs are read", async () => {
    const other = crypto.randomUUID();
    const { deps } = world({ tenantHeader: other });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(`a line of the run names tenant ${other}, not tenant A`);
  });

  test("missing token exchanges time out naming the runs and the events awaited", async () => {
    const { deps } = world({ dropExchange: "telemetry-query" });
    await expect(checkAssistantTools(deps, APP, "saas", crypto.randomUUID())).rejects.toThrow(
      /timed out after 120 s waiting for the assistant's "question run started" and the identity service's "issued a run delegation" and "exchanged a run delegation" lines of runs .*; last: run 1: 1 started, 1 issued, exchanged assets ×1/,
    );
  });
});

describe("the calls the check plans", () => {
  const { a, b } = fixedData();
  const times = readingTimes(START);

  test("check 1 calls every tool on the asker's own device, each expected ok with the asker's data", () => {
    const plan = ownPlan(a, times.from, times.to);
    expect(plan.map((p) => p.call.name)).toEqual([...ALL_TOOLS]);
    expect(plan.every((p) => p.expect.kind === "holds" && p.expect.texts.length > 0)).toBe(true);
  });

  test("check 2 and its control make the same calls; as another tenant each telemetry tool must be ok and empty, never denied", () => {
    const check2 = crossPlan(a, b, times.from, times.to);
    const control = crossPlan(b, b, times.from, times.to);
    expect(check2.map((p) => p.call)).toEqual(control.map((p) => p.call));
    expect(check2.length).toBeLessThanOrEqual(8);
    expect(check2.map((p) => [p.label, p.expect.kind])).toEqual([
      ["telemetry_latest (B's device)", "empty"],
      ["telemetry_statistics (B's device)", "empty"],
      ["telemetry_usage (B's device)", "empty"],
      ["telemetry_sql (B's device)", "empty"],
      ["telemetry_sql (no device filter)", "holds"],
      ["telemetry_sql (trustiot.telemetry)", "refused"],
    ]);
    expect(control.map((p) => p.expect.kind)).toEqual(["holds", "holds", "holds", "holds", "holds", "refused"]);
    // The SQL without a filter holds the asker's own text reading, A's in check 2 and B's in the control.
    expect(check2[4]?.expect).toEqual({ kind: "holds", texts: [a.text] });
    expect(control[4]?.expect).toEqual({ kind: "holds", texts: [b.text] });
    // Every telemetry tool, whatever the device.
    expect(new Set(check2.map((p) => p.call.name))).toEqual(new Set(TELEMETRY_TOOLS));
  });

  test("the SQL without a device filter reads the tenant view by the text readings' prefix; the other reads the base table", () => {
    expect(PREFIX_SQL).toBe("SELECT point, value_string FROM tenant_telemetry WHERE startsWith(value_string, 'nightly-t-') ORDER BY ts");
    expect(PREFIX_SQL).not.toMatch(/device_id/);
    expect(BASE_TABLE_SQL).toBe("SELECT point, value_string FROM trustiot.telemetry WHERE startsWith(value_string, 'nightly-t-')");
    for (const data of [a, b, newTenantData("A")]) expect(data.text.startsWith("nightly-t-")).toBe(true);
  });
});

describe("the smoke cannot pass a shape that runs the assistant without the whole tool check", () => {
  const assistant: K8sObject = { kind: "Deployment", metadata: { name: "trustiot-assistant", labels: { "app.kubernetes.io/component": "assistant" } } };
  const passed = (name: string, detail = "done"): Check => ({ name, ok: true, detail, durationMs: 1 });
  const complete = EVIDENCE_STEPS.map((step) => `saas ${step}: …`).join("; ");

  test("both checks passed once, with every evidence step: no problem", () => {
    expect(toolsCheckProblems([passed("other"), passed(STAND_IN_CHECK), passed(TOOLS_CHECK, complete)], [assistant], "saas")).toEqual([]);
  });

  test("a release without the assistant needs neither", () => {
    expect(toolsCheckProblems([], [], "saas")).toEqual([]);
  });

  test("a check that did not run, failed, ran twice or stopped before its last step is a problem", () => {
    expect(toolsCheckProblems([passed("other")], [assistant], "saas")).toEqual([
      "the stand-in's upgrade ran 0 time(s), expected once",
      "the tool check ran 0 time(s), expected once",
      "the tool check's result has no setup, check 1, control, check 2, check 3 of shape saas",
    ]);
    expect(toolsCheckProblems([passed(STAND_IN_CHECK), { ...passed(TOOLS_CHECK, complete), ok: false }], [assistant], "saas")).toEqual([
      "the tool check failed",
      "the tool check's result has no setup, check 1, control, check 2, check 3 of shape saas",
    ]);
    expect(toolsCheckProblems([passed(STAND_IN_CHECK), passed(STAND_IN_CHECK), passed(TOOLS_CHECK, complete)], [assistant], "saas")).toEqual(["the stand-in's upgrade ran 2 time(s), expected once"]);
    const short = EVIDENCE_STEPS.filter((step) => step !== "check 3").map((step) => `saas ${step}: …`).join("; ");
    expect(toolsCheckProblems([passed(STAND_IN_CHECK), passed(TOOLS_CHECK, short)], [assistant], "saas")).toEqual(["the tool check's result has no check 3 of shape saas"]);
    // Another shape's evidence does not count.
    expect(toolsCheckProblems([passed(STAND_IN_CHECK), passed(TOOLS_CHECK, complete)], [assistant], "onprem")).toEqual([
      "the tool check's result has no setup, check 1, control, check 2, check 3 of shape onprem",
    ]);
  });

  test("smoke.ts runs both checks by these names and the guard last", async () => {
    const smoke = await Bun.file(join(import.meta.dir, "smoke.ts")).text();
    expect(smoke.split("check(STAND_IN_CHECK,").length).toBe(2);
    expect(smoke.split("check(TOOLS_CHECK,").length).toBe(2);
    expect(smoke.split("toolsCheckProblems(checks,").length).toBe(2);
  });
});

describe("the stand-in reads the directives the check writes", () => {
  test("each call comes back as a tool_use block with the input the check wrote, and its lines parse into the record the check reads", async () => {
    const tenant = crypto.randomUUID();
    const device = crypto.randomUUID();
    const tag = "0123abcd4567ef89";
    const times = readingTimes(START);
    const calls = [{ name: MODEL_TOOL, input: {} }, ...telemetryCalls(device, times.from, times.to)];
    const offered = ALL_TOOLS.filter((name) => name !== "telemetry_usage").map((name) => ({ name, description: name, input_schema: { type: "object" } }));
    const request = (messages: Json[]): RawRequest => ({
      target: "/internal/roles/smoke/v1/messages",
      method: "POST",
      hostHeader: "trustiot-ai-gateway:8080",
      headers: [
        ["content-type", "application/json"],
        ["ai-gateway-tenant", tenant],
      ],
      body: JSON.stringify({ max_tokens: 16000, tools: offered, messages }),
    });
    const question = { role: "user", content: directive(tag, calls) };
    const first = JSON.parse((await double.send(request([question]))).body) as { stop_reason: string; content: Json[] };
    expect(first.stop_reason).toBe("tool_use");
    expect(first.content).toEqual(
      calls.flatMap((call, index) => (call.name === "telemetry_usage" ? [] : [{ type: "tool_use", id: `toolu_nightly_${tag}_${index}`, name: call.name, input: call.input }])),
    );
    const results = first.content.map((use) => ({ type: "tool_result", tool_use_id: use.id, content: [{ type: "text", text: JSON.stringify({ rows: [], total: 0, tool: use.name }) }] }));
    const second = JSON.parse((await double.send(request([question, { role: "assistant", content: first.content }, { role: "user", content: results }]))).body) as { content: Json[] };
    expect(String(second.content[0]?.text)).toStartWith("What the tools returned:\n\n1. device_models, ok: ");
    await double.waitForLine("the results' request line", (line) => typeof line === "object" && line !== null && (line as Json).tag === tag && (line as Json).turn === "tool_results");
    const record = standInRecord(standInEvents(await standInLinesOf(tag)), tag);
    expect(record?.question).toEqual({ tenant, offered: offered.map((t) => t.name), requested: calls.map((c) => c.name), skipped: ["telemetry_usage"] });
    expect(record?.resultsTenant).toBe(tenant);
    expect(record?.results.map((r) => r.tool)).toEqual([MODEL_TOOL, "telemetry_latest", "telemetry_statistics", "telemetry_sql"]);
    expect(record?.results.every((r) => r.tenant === tenant && !r.isError)).toBe(true);
  });

  test("a directive has 1 to 8 calls and a tag of lower-case letters and digits", () => {
    expect(() => directive("Not-A-Tag", [{ name: MODEL_TOOL, input: {} }])).toThrow("not a directive tag");
    expect(() => directive("abc", [])).toThrow("1 to 8 calls");
    expect(() => directive("abc", Array.from({ length: 9 }, () => ({ name: MODEL_TOOL, input: {} })))).toThrow("1 to 8 calls");
    expect(directive("abc", [{ name: MODEL_TOOL, input: {} }])).toBe('nightly-tools {"tag":"abc","calls":[{"name":"device_models","input":{}}]}');
  });
});

describe("the data", () => {
  test("canaries: exact numbers as JavaScript prints them, distinct within a tenant", () => {
    for (let i = 0; i < 50; i++) {
      const data = newTenantData("A");
      expect(data.modelName).toMatch(/^nightly-m-[0-9a-f]{16}$/);
      expect(data.modelName.length).toBeLessThanOrEqual(40);
      expect(data.text).toMatch(/^nightly-t-[0-9a-f]{16}$/);
      const canaries = canariesOf(data);
      expect(canaries[2]).toMatch(/^\d{6}\.625$/);
      expect(canaries[4]).toMatch(/^\d+\.25$/);
      expect(canaries[5]).toMatch(/^\d{6}\.75$/);
      expect(new Set(canaries).size).toBe(canaries.length);
    }
  });

  test("a pair whose canaries clash is made again; none that tells the tenants apart in 100 attempts fails", () => {
    const fixed = fixedData();
    expect(canaryClashes(fixed.a, fixed.b)).toEqual([]);
    expect(canaryClashes(fixed.a, { ...fixed.b, first: fixed.a.first })).toContain(`${fixed.a.first} and ${fixed.a.first}`);
    let made = 0;
    const pair = tenantPair((label) => {
      made += 1;
      return made <= 2 ? { ...fixed.a, label } : label === "A" ? fixed.a : fixed.b;
    });
    expect(made).toBe(4);
    expect(pair.b.modelName).toBe(fixed.b.modelName);
    expect(() => tenantPair((label) => ({ ...fixed.a, label }))).toThrow("in 100 attempts");
  });

  test("the readings' INSERT: three rows of one batch, the numbers a minute apart, built from checked values only", () => {
    const tenant = crypto.randomUUID();
    const { a } = fixedData();
    const firstAt = new Date(Date.UTC(2026, 10, 4, 9, 57));
    const sql = telemetryRowsSql(tenant, a, "nightly-0123456789abcdef", firstAt, new Date(Date.UTC(2026, 10, 4, 10, 0, 30, 5)));
    expect(sql).toStartWith("INSERT INTO trustiot.telemetry (tenant_id, gateway_id, stream_id, batch_seq, device_id, point, ts, received_at, value_type, value_float, value_string) VALUES\n");
    expect(sql).toContain(`('${tenant}', 'nightly-smoke', 'nightly-0123456789abcdef', 1, '${a.device}', 'nightly_number', '2026-11-04 09:57:00.000', '2026-11-04 10:00:30.005', 'float', 123456.625, '')`);
    expect(sql).toContain(`'nightly_number', '2026-11-04 09:58:00.000', '2026-11-04 10:00:30.005', 'float', 125457.875, '')`);
    expect(sql).toContain(`'nightly_text', '2026-11-04 09:58:00.000', '2026-11-04 10:00:30.005', 'string', 0, '${a.text}')`);
    expect(() => telemetryRowsSql("tenant'; DROP TABLE x; --", a, "nightly-0123456789abcdef", firstAt, firstAt)).toThrow("not a UUID");
    expect(() => telemetryRowsSql(tenant, { ...a, device: "x" }, "nightly-0123456789abcdef", firstAt, firstAt)).toThrow("not a UUID");
    expect(() => telemetryRowsSql(tenant, a, "other", firstAt, firstAt)).toThrow("not a stream ID");
    expect(() => telemetryRowsSql(tenant, { ...a, text: "it's" }, "nightly-0123456789abcdef", firstAt, firstAt)).toThrow("not a text reading");
    expect(() => telemetryRowsSql(tenant, { ...a, first: Number.NaN }, "nightly-0123456789abcdef", firstAt, firstAt)).toThrow("not a reading");
    expect(() => telemetryRowsSql(tenant, { ...a, last: 1e21 }, "nightly-0123456789abcdef", firstAt, firstAt)).toThrow("not a reading");
  });

  test("the range the tools ask for holds both minutes of readings, on whole minutes, in RFC 3339 UTC", () => {
    const times = readingTimes(START);
    expect(times.firstAt.toISOString()).toBe("2026-11-04T09:57:00.000Z");
    expect(times.from).toBe("2026-11-04T09:55:00Z");
    expect(times.to).toBe("2026-11-04T10:02:00Z");
  });

  test("the telemetry calls name the device in every input and read the tenant view in SQL", () => {
    const device = crypto.randomUUID();
    const calls = telemetryCalls(device, "2026-11-04T09:55:00Z", "2026-11-04T10:02:00Z");
    expect(calls.map((c) => c.name)).toEqual([...TELEMETRY_TOOLS]);
    for (const call of calls) expect(JSON.stringify(call.input)).toContain(device);
    expect(String(calls[3]?.input.sql)).toContain("FROM tenant_telemetry WHERE device_id = ");
    expect(() => telemetryCalls("x", "a", "b")).toThrow("not a UUID");
  });

  test("a result's shape names keys, statuses and counts, never a value", () => {
    expect(resultShape("telemetry_latest", JSON.stringify({ device_id: "d", rows: [["p", "t", "nightly-t-secret"]], total: 1 }))).toEqual({ empty: false, shape: "{device_id,rows,total} with 1 row(s), total 1" });
    expect(resultShape("telemetry_latest", JSON.stringify({ rows: [], total: 0 })).empty).toBe(true);
    expect(resultShape("telemetry_statistics", JSON.stringify({ series: [{ status: "no_data" }, { status: "no_data" }] }))).toEqual({ empty: true, shape: "{series} with series no_data,no_data" });
    expect(resultShape("telemetry_usage", JSON.stringify({ series: [{ status: "ok", total: 3 }] })).empty).toBe(false);
    expect(resultShape("telemetry_usage", JSON.stringify({ series: [] })).empty).toBe(false);
    expect(resultShape("telemetry_sql", JSON.stringify({ rows: [], row_count: 0 }))).toEqual({ empty: true, shape: "{row_count,rows} with 0 row(s), row_count 0" });
    expect(resultShape("telemetry_sql", "nightly-t-secret is not JSON")).toEqual({ empty: false, shape: "not JSON (28 characters)" });
  });
});

describe("the logs, without kubectl logs", () => {
  test("the services' events come from the collector's log table, selected by event and run ID only", () => {
    const runs = [crypto.randomUUID(), crypto.randomUUID()];
    const sql = serviceEventsSql(runs);
    expect(sql).toBe(
      [
        "SELECT ServiceName AS service, Body AS message, LogAttributes AS fields",
        "FROM trustiot_logs.otel_logs",
        "WHERE Body IN ('question run started', 'tool call completed', 'issued a run delegation', 'exchanged a run delegation')",
        `  AND (LogAttributes['run_id'] IN ('${runs[0]}', '${runs[1]}') OR LogAttributes['run'] IN ('${runs[0]}', '${runs[1]}'))`,
        "ORDER BY Timestamp",
        "FORMAT JSONEachRow",
        "",
      ].join("\n"),
    );
    expect(() => serviceEventsSql([])).toThrow("no run");
    expect(() => serviceEventsSql(["x') OR 1=1 --"])).toThrow("not a UUID");
  });

  test("the table's rows parse into events; anything else fails without its text", () => {
    const row = { service: "identity", message: "exchanged a run delegation", fields: { holder: "assistant", audience: "assets", run: "r" } };
    expect(parseServiceEvents(`${JSON.stringify(row)}\n\n`)).toEqual([row]);
    expect(parseServiceEvents("")).toEqual([]);
    expect(() => parseServiceEvents("Code: 60. DB::Exception: secret-looking text")).toThrow("is not JSON (44 characters)");
    expect(() => parseServiceEvents(JSON.stringify({ name: "x" }))).toThrow("is not an event (keys name)");
  });

  test("the stand-in's lines become events; other lines are skipped", () => {
    expect(standInEvents([{ event: "tool_result", tag: "a" }, "text", { other: 1 }, null])).toEqual([{ service: AI_GATEWAY, message: "tool_result", fields: { event: "tool_result", tag: "a" } }]);
  });
});

describe("the stand-in in place of the gateway", () => {
  const deployment = (image: string, extra: Json = {}): K8sObject => ({
    kind: "Deployment",
    metadata: { name: "trustiot-ai-gateway", labels: { "app.kubernetes.io/component": AI_GATEWAY } },
    spec: {
      selector: { matchLabels: { "app.kubernetes.io/name": "trustiot", "app.kubernetes.io/instance": "trustiot", "app.kubernetes.io/component": AI_GATEWAY } },
      template: { spec: { containers: [{ name: AI_GATEWAY, image, imagePullPolicy: image === AI_GATEWAY_DOUBLE_IMAGE ? "Never" : "IfNotPresent", ...extra }] } },
    },
  });
  const other: K8sObject = { kind: "ConfigMap", metadata: { name: "trustiot-ai-gateway" }, data: { A: "1" } };

  test("only the gateway container's image and pull policy may change", () => {
    const before = [deployment("localhost/trustiot-e2e/ai-gateway:nightly"), other];
    expect(releaseChangeProblems(before, [deployment(AI_GATEWAY_DOUBLE_IMAGE), other])).toEqual([]);
    expect(releaseChangeProblems(before, before)).toEqual([
      "Deployment/trustiot-ai-gateway runs localhost/trustiot-e2e/ai-gateway:nightly with pull policy IfNotPresent, not localhost/trustiot-e2e/ai-gateway-double:nightly with Never",
    ]);
    expect(releaseChangeProblems(before, [deployment(AI_GATEWAY_DOUBLE_IMAGE, { env: [{ name: "X", value: "1" }] }), other])).toEqual([
      "Deployment/trustiot-ai-gateway changed beyond its container's image and pull policy",
    ]);
    expect(releaseChangeProblems(before, [deployment(AI_GATEWAY_DOUBLE_IMAGE), { ...other, data: { A: "2" } }])).toEqual(["ConfigMap/trustiot-ai-gateway changed"]);
    expect(releaseChangeProblems(before, [deployment(AI_GATEWAY_DOUBLE_IMAGE)])).toEqual(["ConfigMap/trustiot-ai-gateway is gone"]);
    expect(releaseChangeProblems([other], [other])).toEqual(["the release has 0 ai-gateway Deployments, expected 1"]);
  });

  test("the migration Job the chart runs again for the new image, and the services that wait for it, are expected; nothing else of them", () => {
    const old = "localhost/trustiot-e2e/ai-gateway:nightly";
    const job = (hash: string, gateway: string, assets = "localhost/trustiot-e2e/assets:nightly"): K8sObject => ({
      kind: "Job",
      metadata: { name: `trustiot-migrate-${hash}`, labels: { "app.kubernetes.io/component": "migrate" } },
      spec: { template: { metadata: { annotations: { [SERVICE_IMAGES_ANNOTATION]: `ai-gateway=${gateway},assets=${assets}` } }, spec: { containers: [{ name: "migrate" }] } } },
    });
    const waiter = (hash: string): K8sObject => ({
      kind: "Deployment",
      metadata: { name: "trustiot-assets", labels: { "app.kubernetes.io/component": "assets" } },
      spec: { template: { spec: { initContainers: [{ name: "wait-for-migrations", env: [{ name: "TRUSTIOT__KUBE__JOB", value: `trustiot-migrate-${hash}` }] }] } } },
    });
    const role = (hash: string): K8sObject => ({ kind: "Role", metadata: { name: "trustiot-migrate-status" }, rules: [{ resourceNames: [`trustiot-migrate-${hash}`] }] });
    const gateway = (image: string) => {
      const d = deployment(image);
      const spec = (d.spec?.template as { spec: Json }).spec;
      spec.initContainers = [{ name: "wait-for-migrations", env: [{ name: "TRUSTIOT__KUBE__JOB", value: `trustiot-migrate-${image === old ? "0123abcd" : "4567ef89"}` }] }];
      return d;
    };
    const before = [job("0123abcd", old), waiter("0123abcd"), role("0123abcd"), gateway(old)];
    expect(releaseChange(before, [job("4567ef89", AI_GATEWAY_DOUBLE_IMAGE), waiter("4567ef89"), role("4567ef89"), gateway(AI_GATEWAY_DOUBLE_IMAGE)])).toEqual({
      problems: [],
      migrateJob: { before: "trustiot-migrate-0123abcd", after: "trustiot-migrate-4567ef89" },
      waiting: ["Deployment/trustiot-assets"],
    });
    expect(releaseChangeProblems(before, [job("4567ef89", AI_GATEWAY_DOUBLE_IMAGE, "localhost/trustiot-e2e/assets:other"), waiter("4567ef89"), role("4567ef89"), gateway(AI_GATEWAY_DOUBLE_IMAGE)])).toEqual([
      `the migration Job's ${SERVICE_IMAGES_ANNOTATION} names ${AI_GATEWAY_DOUBLE_IMAGE} for ai-gateway and changed for assets, expected only ai-gateway=${AI_GATEWAY_DOUBLE_IMAGE}`,
      "Job/trustiot-migrate-0123abcd changed",
    ]);
    const changedWaiter = waiter("4567ef89");
    ((changedWaiter.spec?.template as { spec: Json }).spec as Json).containers = [{ name: "assets", image: "elsewhere" }];
    expect(releaseChangeProblems(before, [job("4567ef89", AI_GATEWAY_DOUBLE_IMAGE), changedWaiter, role("4567ef89"), gateway(AI_GATEWAY_DOUBLE_IMAGE)])).toEqual(["Deployment/trustiot-assets changed"]);
    expect(releaseChangeProblems(before, [waiter("4567ef89"), role("4567ef89"), gateway(AI_GATEWAY_DOUBLE_IMAGE)])).toEqual([
      "the release has 1 migration Job(s) before and 0 after, expected one or none both times",
      "Job/trustiot-migrate-0123abcd is gone",
      "Deployment/trustiot-assets changed",
      "Role/trustiot-migrate-status changed",
      "Deployment/trustiot-ai-gateway changed beyond its container's image and pull policy",
    ]);
  });

  test("the dry run's manifest is the release's in helm's JSON; anything else fails without its text", () => {
    const manifest = "---\nkind: ConfigMap\nmetadata:\n  name: x\n";
    expect(dryRunManifest(JSON.stringify({ name: "trustiot", manifest, config: { secret: "s" } }))).toBe(manifest);
    expect(() => dryRunManifest("Error: UPGRADE FAILED: secret-looking text")).toThrow("did not print JSON (42 characters)");
    expect(() => dryRunManifest(JSON.stringify({ name: "trustiot", info: {} }))).toThrow("printed no manifest (keys info,name)");
    expect(() => dryRunManifest(JSON.stringify({ manifest: " " }))).toThrow("printed no manifest (keys manifest)");
    expect(() => dryRunManifest("[]")).toThrow("printed no manifest (keys none)");
  });

  test("the assistant registers device_models with the assets URL and the four telemetry tools with telemetry-query's", () => {
    const assistant = (env: string[]): K8sObject => ({
      kind: "Deployment",
      metadata: { name: "trustiot-assistant", labels: { "app.kubernetes.io/component": "assistant" } },
      spec: { template: { spec: { containers: [{ name: "assistant", env: env.map((name) => ({ name, value: "http://x:8080" })) }] } } },
    });
    expect(registeredTools([assistant(["TRUSTIOT__ASSETS__URL", "TRUSTIOT__TELEMETRY_QUERY__URL"])])).toEqual([...ALL_TOOLS]);
    expect(registeredTools([assistant(["TRUSTIOT__TELEMETRY_QUERY__URL"])])).toEqual([...TELEMETRY_TOOLS]);
    expect(registeredTools([assistant([])])).toEqual([]);
    expect(() => registeredTools([other])).toThrow("one assistant container, found 0");
  });

  test("the ClickHouse client gets the administrator's password from the file in its environment, never as an argument", () => {
    expect(CLICKHOUSE_ADMIN).not.toContain("--password");
    const dir = mkdtempSync(join(tmpdir(), "nightly-clickhouse-client-"));
    try {
      // A stand-in for clickhouse-client that prints its arguments, the password it got and its input.
      const client = join(dir, "clickhouse-client");
      writeFileSync(client, '#!/bin/sh\nprintf "args=%s\\n" "$*"\nprintf "password=%s\\n" "$CLICKHOUSE_PASSWORD"\ncat\n', { mode: 0o755 });
      const password = join(dir, "password");
      writeFileSync(password, "pw-from-file\n", { mode: 0o400 });
      const runScript = (file: string) =>
        Bun.spawnSync({
          cmd: ["bash", "-c", CLICKHOUSE_ADMIN],
          env: { PATH: `${dir}:/usr/bin:/bin`, CLICKHOUSE_USER: "trustiot_admin", CLICKHOUSE_PASSWORD_FILE: file },
          stdin: new Blob(["SELECT 1\n"]),
          stdout: "pipe",
          stderr: "pipe",
          timeout: COMMAND_TIMEOUT_MS,
        });
      const ran = runScript(password);
      expect({ code: ran.exitCode, out: ran.stdout.toString() }).toEqual({ code: 0, out: "args=--user trustiot_admin\npassword=pw-from-file\nSELECT 1\n" });
      // Without the file the client never starts.
      const missing = runScript(join(dir, "absent"));
      expect({ code: missing.exitCode, out: missing.stdout.toString() }).toEqual({ code: 1, out: "" });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("the overlay names the stand-in's image, never pulled", () => {
    expect(aiGatewayDoubleValues()).toEqual({
      services: { [AI_GATEWAY]: { image: { repository: "localhost/trustiot-e2e/ai-gateway-double", tag: "nightly", digest: "", pullPolicy: "Never" } } },
    });
  });

  test("the pods' selector comes from the Deployment's matchLabels", () => {
    expect(selectorOf([deployment("x"), other], AI_GATEWAY)).toBe("app.kubernetes.io/component=ai-gateway,app.kubernetes.io/instance=trustiot,app.kubernetes.io/name=trustiot");
    expect(() => selectorOf([other], AI_GATEWAY)).toThrow("one Deployment of ai-gateway, found 0");
  });

  const pod = (name: string, image: string, ready: boolean, terminating = false): Json => ({
    metadata: { name, ...(terminating ? { deletionTimestamp: "2026-11-04T10:00:00Z" } : {}) },
    spec: { containers: [{ name: AI_GATEWAY, image }] },
    status: { conditions: [{ type: "Ready", status: ready ? "True" : "False" }] },
  });

  test("the rollout is done when every pod runs the stand-in and is ready, terminating old pods included", async () => {
    const lists = [
      { items: [pod("old", "localhost/trustiot-e2e/ai-gateway:nightly", true), pod("new", AI_GATEWAY_DOUBLE_IMAGE, false)] },
      { items: [pod("old", "localhost/trustiot-e2e/ai-gateway:nightly", false, true), pod("new", AI_GATEWAY_DOUBLE_IMAGE, true)] },
      { items: [pod("new", AI_GATEWAY_DOUBLE_IMAGE, true)] },
    ];
    let clock = 0;
    let call = 0;
    const done = await waitForAiGatewayDouble({ pods: async () => lists[Math.min(call++, lists.length - 1)], sleep: async (ms) => void (clock += ms), now: () => clock });
    expect(call).toBe(3);
    expect(done).toBe("1 ai-gateway pod(s) run localhost/trustiot-e2e/ai-gateway-double:nightly and are ready, no other pod left after 4 s");
    expect(componentPods(lists[1], AI_GATEWAY)).toEqual([
      { name: "old", image: "localhost/trustiot-e2e/ai-gateway:nightly", ready: false, terminating: true },
      { name: "new", image: AI_GATEWAY_DOUBLE_IMAGE, ready: true, terminating: false },
    ]);
  });

  test("a rollout that never finishes times out naming what was awaited and the pods last seen", async () => {
    let clock = 0;
    const stuck = waitForAiGatewayDouble({ pods: async () => ({ items: [pod("new", AI_GATEWAY_DOUBLE_IMAGE, false)] }), sleep: async (ms) => void (clock += ms), now: () => clock });
    await expect(stuck).rejects.toThrow(
      "timed out after 420 s waiting for every ai-gateway pod to run localhost/trustiot-e2e/ai-gateway-double:nightly and be ready, with no pod of another image left; last: new localhost/trustiot-e2e/ai-gateway-double:nightly",
    );
    let empty = 0;
    await expect(waitForAiGatewayDouble({ pods: async () => ({ items: [] }), sleep: async (ms) => void (empty += ms), now: () => empty })).rejects.toThrow("last: no pod");
  });
});

describe("the stand-in's values on each deployment shape", () => {
  for (const shape of discoverShapes(CHART_DIR)) {
    // Three commands (resolving the pinned helm, two renders), each with its own limit: the test's
    // limit covers them on a loaded machine (CLAUDE.md, "Test time limits"). The second render is
    // helm's own dry run in JSON, so dryRunManifest reads what the pinned helm prints; the smoke
    // runs it with --dry-run=server against the release, which needs a cluster.
    test(`values-${shape.name}.yaml with the API check's values: the stand-in's file changes only the ai-gateway Deployment's image and pull policy`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const dir = mkdtempSync(join(tmpdir(), "nightly-ai-gateway-double-"));
      try {
        const api = join(dir, "values-api.yaml");
        writeFileSync(api, Bun.YAML.stringify(apiValues(shape.name, routesOfValues(CHART_DIR, shape.valuesFiles)), null, 2));
        const overlay = join(dir, "values-ai-gateway-double.yaml");
        writeFileSync(overlay, Bun.YAML.stringify(aiGatewayDoubleValues(), null, 2));
        const render = (command: string[], extra: string[]) => {
          const cmd = [helm, ...command, "trustiot", CHART_DIR, "--namespace", `nightly-${shape.name}`, ...valuesArgs(CHART_DIR, shape), "-f", api, ...extra];
          const rendered = Bun.spawnSync({ cmd, stdout: "pipe", stderr: "pipe", timeout: COMMAND_TIMEOUT_MS });
          throwIfTimedOut(cmd, rendered);
          expect(rendered.stderr.toString()).toBe("");
          expect(rendered.exitCode, `helm ${command[0]} exit code`).toBe(0);
          return rendered.stdout.toString();
        };
        const before = parseManifests(render(["template"], []));
        const after = parseManifests(dryRunManifest(render(["install", "--dry-run=client", "--output", "json"], ["-f", overlay])));
        const change = releaseChange(before, after);
        expect(change.problems).toEqual([]);
        // The AI gateway uses Postgres, so its new image runs the migration Job again and the
        // services that use Postgres wait for that run (templates/_migrate.tpl).
        expect(change.migrateJob?.before).toMatch(/^trustiot-migrate-[0-9a-f]{8}$/);
        expect(change.migrateJob?.after).not.toBe(change.migrateJob?.before);
        expect(change.waiting).toEqual(expect.arrayContaining(["Deployment/trustiot-assistant", "Deployment/trustiot-identity", "Deployment/trustiot-assets", "Deployment/trustiot-telemetry-query"]));
        // The policies that admit the assistant to the gateway are the production ones: the
        // stand-in keeps the workload's name, its ServiceAccount and its labels.
        const policies = (objects: K8sObject[]) => objects.filter((o) => o.kind === "AuthorizationPolicy" || o.kind === "NetworkPolicy").map((o) => JSON.stringify(o));
        expect(policies(after)).toEqual(policies(before));
        expect(selectorOf(after, AI_GATEWAY)).toBe(selectorOf(before, AI_GATEWAY));
        // Where the check reads the logs: the stand-in's lines port, which the chart declares
        // nowhere (so no Service or NetworkPolicy names it; the stand-in binds it to the loopback),
        // next to the health port the chart probes (the stand-in's HEALTH_PORT default), and the
        // collector's log table, into which the assistant and the identity service export their
        // events under their own names (serviceEventsSql).
        const ports = containerPortsOf(after, AI_GATEWAY);
        expect(ports.health).toBe(8081);
        expect(Object.values(ports)).not.toContain(STAND_IN_LINES_PORT);
        expect(JSON.stringify(after).match(new RegExp(`"(containerPort|port|targetPort)":${STAND_IN_LINES_PORT}\\b`, "g"))).toBeNull();
        // The release's assistant registers every tool the check calls.
        expect(registeredTools(after)).toEqual([...ALL_TOOLS]);
        expect(after.some((o) => o.kind === "Deployment" && o.metadata?.labels?.["app.kubernetes.io/component"] === "otel-collector")).toBe(true);
        for (const service of ["assistant", "identity"]) {
          const container = asContainers(after.find((o) => o.kind === "Deployment" && o.metadata?.name === `trustiot-${service}`)).find((c) => c.name === service);
          const env = Object.fromEntries((container?.env ?? []).map((e) => [e.name, e.value]));
          expect({ service, name: env.OTEL_SERVICE_NAME, exported: env.TRUSTIOT__TELEMETRY__OTLP_ENDPOINT?.startsWith("http://") }).toEqual({ service, name: service, exported: true });
        }
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    }, SLOW_TEST_MS);
  }
});
