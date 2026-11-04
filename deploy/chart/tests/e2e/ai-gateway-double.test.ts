// The AI gateway stand-in (ai-gateway-double/server.ts, Laysi/trustiot-v2#648) without a cluster:
// it runs as a Bun child process on free loopback ports, and the test sends it requests shaped
// like the assistant's (services/assistant/src/model/anthropic.rs, request_body and
// tool_results_message): the question's turn, the tool results' turn, the role and tenant header
// checks, the health and admin ports, and the lines it prints for the nightly.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { useProcessTestTimeouts } from "../timing.ts";
import { type AiGatewayDouble, startAiGatewayDouble } from "./ai-gateway-double-process.ts";
import type { RawRequest, RawResponse } from "./raw-client.ts";

useProcessTestTimeouts();

const TENANT = crypto.randomUUID();
const TAG = crypto.randomUUID().replaceAll("-", "").slice(0, 16);
const DEVICE = crypto.randomUUID();
const CANARY = `nightly-t-${TAG}`;

let double: AiGatewayDouble;

beforeAll(async () => {
  double = await startAiGatewayDouble();
});

afterAll(() => {
  double?.stop();
});

type Json = Record<string, unknown>;

/** A tool as the assistant describes it (ToolSpec): name, description, input schema. */
const tool = (name: string): Json => ({ name, description: `The ${name} tool.`, input_schema: { type: "object", properties: {}, additionalProperties: false } });

/** The body request_body writes: no model and no stream, the tools, the history. */
function body(messages: Json[], tools = ["device_models", "telemetry_latest", "telemetry_statistics"]): Json {
  return {
    max_tokens: 16000,
    thinking: { type: "adaptive" },
    output_config: { effort: "high" },
    cache_control: { type: "ephemeral" },
    system: [{ type: "text", text: "You are the TrustIoT assistant." }],
    tools: tools.map(tool),
    messages,
  };
}

function post(payload: unknown, options: { role?: string; tenant?: string[]; method?: string; target?: string } = {}): RawRequest {
  return {
    target: options.target ?? `/internal/roles/${options.role ?? "smoke"}/v1/messages`,
    method: options.method ?? "POST",
    hostHeader: "trustiot-ai-gateway:8080",
    headers: [["content-type", "application/json"], ...(options.tenant ?? [TENANT]).map((value): [string, string] => ["ai-gateway-tenant", value])],
    body: typeof payload === "string" ? payload : JSON.stringify(payload),
  };
}

const directive = (calls: { name: string; input: Json }[], tag = TAG) => `nightly-tools ${JSON.stringify({ tag, calls })}`;
const question = (text: string): Json => ({ role: "user", content: text });

function parsed(response: RawResponse): Json {
  return JSON.parse(response.body) as Json;
}

/** The required fields of a Messages response the assistant deserializes, and the gateway's headers. */
function expectMessage(response: RawResponse, stopReason: string): Json {
  expect(response.status).toBe(200);
  expect(response.headers["content-type"]).toEqual(["application/json"]);
  expect(response.headers["ai-gateway-model"]).toEqual(["anthropic/claude-smoke"]);
  const message = parsed(response);
  expect(message).toMatchObject({ type: "message", role: "assistant", stop_reason: stopReason, stop_sequence: null });
  expect(message.id).toMatch(/^msg_nightly_\d+$/);
  expect(typeof message.model).toBe("string");
  expect(Array.isArray(message.content)).toBe(true);
  const usage = message.usage as { input_tokens: number; output_tokens: number };
  expect(usage.input_tokens).toBeGreaterThan(0);
  expect(usage.output_tokens).toBeGreaterThan(0);
  return message;
}

const isEvent = (event: string, extra: (line: Json) => boolean = () => true) => (line: unknown) =>
  typeof line === "object" && line !== null && (line as Json).event === event && extra(line as Json);

describe("the AI gateway stand-in's script", () => {
  const calls = [
    { name: "device_models", input: {} },
    { name: "telemetry_latest", input: { device_id: DEVICE } },
    { name: "telemetry_sql", input: { sql: `SELECT point FROM tenant_telemetry WHERE device_id = '${DEVICE}'` } },
  ];

  test("a directive gets one tool_use block per offered tool, in order, with its input; a tool not offered is skipped and logged", async () => {
    const response = await double.send(post(body([question(directive(calls))])));
    const message = expectMessage(response, "tool_use");
    expect(message.content).toEqual([
      { type: "tool_use", id: `toolu_nightly_${TAG}_0`, name: "device_models", input: {} },
      { type: "tool_use", id: `toolu_nightly_${TAG}_1`, name: "telemetry_latest", input: { device_id: DEVICE } },
    ]);
    const line = await double.waitForLine("the question's request line", isEvent("request", (l) => l.tag === TAG && l.turn === "question"));
    expect(line).toEqual({
      event: "request",
      tenant: TENANT,
      role: "smoke",
      turn: "question",
      tag: TAG,
      offered: ["device_models", "telemetry_latest", "telemetry_statistics"],
      requested: ["device_models", "telemetry_latest", "telemetry_sql"],
      skipped: ["telemetry_sql"],
    });
  });

  test("tool results get one paragraph each that repeats the text on one line, safe from the answer guard, and one log line each with the raw text", async () => {
    const asked = expectMessage(await double.send(post(body([question(directive(calls))]))), "tool_use");
    const latest = JSON.stringify({ device_id: DEVICE, columns: ["point", "time", "value"], rows: [["nightly_text", "2026-11-04T00:00:00Z", CANARY]], total: 1 });
    const odd = "line one\nline `two` with <b>a tag</b>";
    const results = {
      role: "user",
      content: [
        { type: "tool_result", tool_use_id: `toolu_nightly_${TAG}_1`, content: [{ type: "text", text: latest }] },
        { type: "tool_result", tool_use_id: `toolu_nightly_${TAG}_0`, content: [{ type: "text", text: odd }], is_error: true },
      ],
    };
    // A system note may follow the results (the assistant's wind-down); the script skips it.
    const note = { role: "system", content: "Answer now." };
    const response = await double.send(post(body([question(directive(calls)), { role: "assistant", content: asked.content }, results, note])));
    const message = expectMessage(response, "end_turn");
    const content = message.content as Json[];
    expect(content).toHaveLength(1);
    const text = content[0]?.text as string;
    expect(content[0]?.type).toBe("text");
    expect(text).toBe(
      `What the tools returned:\n\n1. telemetry_latest, ok: ${latest}\n\n2. device_models, error: line one line 'two' with ‹b>a tag‹/b>`,
    );
    expect(text).not.toContain("`");
    expect(text).not.toContain("<");
    const first = await double.waitForLine("the first tool_result line", isEvent("tool_result", (l) => l.tool_use_id === `toolu_nightly_${TAG}_1`));
    expect(first).toEqual({ event: "tool_result", tenant: TENANT, tag: TAG, tool_use_id: `toolu_nightly_${TAG}_1`, tool: "telemetry_latest", is_error: false, text: latest });
    const second = await double.waitForLine("the second tool_result line", isEvent("tool_result", (l) => l.tool_use_id === `toolu_nightly_${TAG}_0`));
    expect(second).toEqual({ event: "tool_result", tenant: TENANT, tag: TAG, tool_use_id: `toolu_nightly_${TAG}_0`, tool: "device_models", is_error: true, text: odd });
    await double.waitForLine("the results' request line", isEvent("request", (l) => l.tag === TAG && l.turn === "tool_results"));
  });

  test("a question without a directive, a malformed directive and one whose tools are all missing end the turn with a text", async () => {
    const plain = expectMessage(await double.send(post(body([question("How many meters reported today?")]))), "end_turn");
    expect(plain.content).toEqual([{ type: "text", text: "The nightly stand-in answers nightly-tools directives only." }]);
    const broken = expectMessage(await double.send(post(body([question("nightly-tools {not json")]))), "end_turn");
    expect((broken.content as Json[])[0]?.text).toBe("The nightly directive is not valid: the directive is not JSON.");
    const badTag = expectMessage(await double.send(post(body([question(directive(calls, "Not-A-Tag"))]))), "end_turn");
    expect((badTag.content as Json[])[0]?.text).toContain("needs a tag");
    const missing = expectMessage(await double.send(post(body([question(directive([{ name: "telemetry_usage", input: {} }], `${TAG}x`))], []))), "end_turn");
    expect(missing.content).toEqual([{ type: "text", text: "None of the tools the directive names is offered: telemetry_usage." }]);
    const line = await double.waitForLine("the request line of the directive without offered tools", isEvent("request", (l) => l.tag === `${TAG}x`));
    expect(line).toMatchObject({ offered: [], requested: ["telemetry_usage"], skipped: ["telemetry_usage"] });
  });
});

describe("the AI gateway stand-in's refusals, as the gateway's error envelope", () => {
  const ask = body([question(directive([{ name: "device_models", input: {} }]))]);
  const cases: { name: string; request: RawRequest; status: number; type: string }[] = [
    { name: "another model role", request: post(ask, { role: "main" }), status: 404, type: "not_found_error" },
    { name: "no tenant header", request: post(ask, { tenant: [] }), status: 400, type: "invalid_request_error" },
    { name: "the tenant header twice", request: post(ask, { tenant: [TENANT, crypto.randomUUID()] }), status: 400, type: "invalid_request_error" },
    { name: "a tenant header with a space", request: post(ask, { tenant: ["tenant a"] }), status: 400, type: "invalid_request_error" },
    { name: "a body that is not JSON", request: post("{"), status: 400, type: "invalid_request_error" },
    { name: "a body without messages", request: post({ max_tokens: 1 }), status: 400, type: "invalid_request_error" },
    { name: "a streamed request", request: post({ ...ask, stream: true }), status: 400, type: "invalid_request_error" },
    { name: "an answer as the last message", request: post(body([{ role: "assistant", content: "Hi." }])), status: 400, type: "invalid_request_error" },
    { name: "a GET of the messages path", request: post(ask, { method: "GET" }), status: 405, type: "invalid_request_error" },
    { name: "another path", request: post(ask, { target: "/internal/roles/smoke/v1/decisions" }), status: 404, type: "not_found_error" },
  ];

  test.each(cases)("$name: $status $type", async (c) => {
    const response = await double.send(c.request);
    expect(response.status).toBe(c.status);
    const envelope = parsed(response) as { type: string; error: { type: string; message: string } };
    expect(envelope.type).toBe("error");
    expect(envelope.error.type).toBe(c.type);
    expect(typeof envelope.error.message).toBe("string");
  });

  test("a refusal's line has the reason, never a header value", async () => {
    const refused = (await double.send(post(ask, { tenant: ["tenant a"] }))).status;
    expect(refused).toBe(400);
    const lines = double.lines().filter(isEvent("refused"));
    expect(lines.length).toBeGreaterThan(0);
    for (const line of lines) {
      expect(Object.keys(line as Json).sort()).toEqual(["event", "reason", "status", "type"]);
      expect(JSON.stringify(line)).not.toContain("tenant a");
    }
  });
});

describe("the AI gateway stand-in's other ports", () => {
  test("the health port answers /healthz and /readyz with 200 at once and anything else with 404", async () => {
    for (const target of ["/healthz", "/readyz"]) {
      const response = await double.send({ target, hostHeader: "localhost" }, double.healthPort);
      expect({ target, status: response.status }).toEqual({ target, status: 200 });
    }
    expect((await double.send({ target: "/metrics", hostHeader: "localhost" }, double.healthPort)).status).toBe(404);
    expect((await double.send({ target: "/healthz", method: "POST", hostHeader: "localhost", body: "" }, double.healthPort)).status).toBe(404);
  });

  test("the admin port answers 404 and logs the method and path", async () => {
    const response = await double.send({ target: "/internal/admin/v1/roles", hostHeader: "trustiot-ai-gateway-admin:8082" }, double.adminPort);
    expect(response.status).toBe(404);
    expect(parsed(response)).toMatchObject({ type: "error", error: { type: "not_found_error" } });
    await double.waitForLine("the admin request line", isEvent("admin_request", (l) => l.method === "GET" && l.path === "/internal/admin/v1/roles"));
  });
});
