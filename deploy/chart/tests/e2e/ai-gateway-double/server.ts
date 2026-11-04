// Scripted stand-in of the AI gateway for the nightly smoke (Laysi/trustiot-v2#648). The nightly
// replaces the gateway's image with this one at the end of each shape's smoke, so the assistant's
// questions run through the real assistant, identity, assets and telemetry-query services and the
// mesh, and only the model is scripted. It speaks the gateway's contract as the assistant uses it
// (services/ai-gateway/README.md, 「內部 API」; services/assistant/src/model/anthropic.rs):
//
//   POST /internal/roles/{role}/v1/messages   Anthropic Messages, never streamed
//       the role is not ROLE              -> 404 not_found_error "no such model role"
//       no valid ai-gateway-tenant header -> 400 invalid_request_error
//       a body that is not a request      -> 400 invalid_request_error
//       "stream": true                    -> 400 invalid_request_error (the assistant never streams)
//       otherwise 200 with a message, `ai-gateway-model: anthropic/claude-smoke`, decided from the
//       request alone (below)
//   anything else on PORT                 -> 404 not_found_error
//
// The script reads the last message that is not a system note:
//
// - A question that starts with DIRECTIVE, followed by a JSON object
//   {"tag": "<1-32 lower-case letters or digits>", "calls": [{"name": "<tool>", "input": {...}}]}
//   (1 to MAX_CALLS calls): `stop_reason: tool_use` with one tool_use block per call whose tool
//   the request offers (`tools[].name`), in order, with the directive's input and the ID
//   `toolu_nightly_<tag>_<index>`. A call of a tool the request does not offer is skipped and
//   reported in the log line; when no call is left, `end_turn` with a text that says so.
// - Any other question: `end_turn` with a fixed text.
// - Tool results (`tool_result` blocks): `end_turn` with one paragraph per result that repeats
//   the result's text, so the answer shows what the tools read. The text is kept on one line, and
//   backticks and `<` are replaced, because the assistant's answer guard (services/assistant/src/
//   answer_guard.rs) removes code spans and anything that looks like an HTML tag.
//
// It writes one JSON line to stdout per model request (event "request": the tenant header, the
// role, the turn, the directive's tag, and the tools offered, requested and skipped) and one per
// tool_result block it receives (event "tool_result": the tenant header, the tool use ID, the
// tool's name taken from the earlier assistant turn, is_error, and the result's text), so the
// nightly reads what the tools handed the model, independently of the answer. It logs no other
// header and nothing of a refused request but the reason. The nightly never runs `kubectl logs`
// (scripts/nightly/diagnostics.test.ts), so the stand-in also keeps its recent lines in memory
// (at most KEPT_BYTES of them, the oldest dropped first) and serves the lines of one directive tag
// on its health port: GET /nightly/v1/lines?tag=<tag> answers {"lines": [...]} in the order they
// were written. The nightly reaches it through `kubectl port-forward` to the pod; the chart's
// NetworkPolicy admits only the kubelet's probes to the health port, so nothing in the cluster
// reads it.
//
// Like a TrustIoT service (crates/service-kit) it listens on a health port (HEALTH_PORT, default
// 8081, the chart's healthPort) that answers GET /healthz and GET /readyz with 200 at once, the
// lines above, and everything else with 404. The admin port (ADMIN_PORT, default 8082, the chart's port `admin`)
// answers 404 to everything and logs the method and path (event "admin_request"): the identity
// service calls the real gateway's admin API only for a platform administrator's model role
// request (services/identity/src/ai_model_roles.rs), which the smoke never makes.
//
// Test-only: it answers whatever the question says, so never deploy it outside a test namespace.

const PORT = Number(process.env.PORT ?? "8080");
const HEALTH_PORT = Number(process.env.HEALTH_PORT ?? "8081");
const ADMIN_PORT = Number(process.env.ADMIN_PORT ?? "8082");

/** The only model role it serves: the smoke assistant's (scripts/nightly/api.ts, ASSISTANT_SMOKE_ENV). */
const ROLE = "smoke";
/** What `ai-gateway-model` says answered: the smoke's model of that role (AI_GATEWAY_SMOKE). */
const ANSWERED_BY = "anthropic/claude-smoke";
/** The question prefix of a directive (scripts/nightly/assistant-tools.ts, DIRECTIVE_PREFIX). */
const DIRECTIVE = "nightly-tools ";
/** The assistant runs at most 8 calls of a step (services/assistant/src/config.rs, max_calls_per_step). */
const MAX_CALLS = 8;
/** The gateway's own request limit (services/ai-gateway, max_request_bytes). */
const MAX_REQUEST_BYTES = 32 << 20;

const MESSAGES_PATH = /^\/internal\/roles\/([^/]+)\/v1\/messages$/;
/** The gateway's rule for the tenant header (services/ai-gateway/src/relay.rs). */
const TENANT = /^[A-Za-z0-9._:-]{1,128}$/;
const TAG = /^[a-z0-9]{1,32}$/;
const TOOL_NAME = /^[A-Za-z0-9_-]{1,64}$/;

type Json = Record<string, unknown>;

const isMap = (value: unknown): value is Json => typeof value === "object" && value !== null && !Array.isArray(value);

/** How much of its recent lines the stand-in keeps for GET /nightly/v1/lines: far more than a night's questions, far below the pod's memory limit. */
const KEPT_BYTES = 16 << 20;
const LINES_PATH = "/nightly/v1/lines";

let answered = 0;
const kept: { tag: unknown; text: string }[] = [];
let keptBytes = 0;

function log(entry: Json): void {
  const text = JSON.stringify(entry);
  console.log(text);
  kept.push({ tag: entry.tag, text });
  keptBytes += text.length;
  while (keptBytes > KEPT_BYTES && kept.length > 1) keptBytes -= kept.shift()?.text.length ?? 0;
}

/** GET /nightly/v1/lines?tag=<tag>: the kept lines of one directive tag, as written. */
function linesOf(req: Request, url: URL): Response {
  if (req.method !== "GET") return new Response("only GET is served", { status: 405 });
  const tag = url.searchParams.get("tag");
  if (tag === null || !TAG.test(tag)) return new Response("the tag is missing or invalid", { status: 400 });
  const lines = kept.filter((line) => line.tag === tag).map((line) => line.text);
  return new Response(`{"lines":[${lines.join(",")}]}`, { headers: { "content-type": "application/json", "cache-control": "no-store" } });
}

function json(status: number, body: unknown, extra: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store", ...extra } });
}

/** The gateway's error envelope (services/ai-gateway/src/reject.rs). */
function refuse(status: number, type: string, message: string): Response {
  log({ event: "refused", status, type, reason: message });
  return json(status, { type: "error", error: { type, message } });
}

/** A Messages response: the content, why it stopped, and a usage the size of the exchange. */
function message(content: Json[], stopReason: "tool_use" | "end_turn", requestBytes: number): Response {
  answered += 1;
  const body = {
    id: `msg_nightly_${answered}`,
    type: "message",
    role: "assistant",
    content,
    model: ANSWERED_BY.slice(ANSWERED_BY.indexOf("/") + 1),
    stop_reason: stopReason,
    stop_sequence: null,
    usage: { input_tokens: Math.ceil(requestBytes / 4), output_tokens: Math.ceil(JSON.stringify(content).length / 4) },
  };
  return json(200, body, { "ai-gateway-model": ANSWERED_BY, "ai-gateway-outcome": "completed" });
}

const text = (value: string): Json => ({ type: "text", text: value });

/** The text of a message's or a tool result's content: a string, or its text blocks joined. */
function textOf(content: unknown): string | undefined {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return undefined;
  const parts: string[] = [];
  for (const block of content) {
    if (!isMap(block) || block.type !== "text" || typeof block.text !== "string") return undefined;
    parts.push(block.text);
  }
  return parts.join("");
}

interface Directive {
  tag: string;
  calls: { name: string; input: Json }[];
}

/** The directive in a question, undefined when the question is none, or a reason when it is malformed. */
function directiveOf(question: string): Directive | string | undefined {
  const trimmed = question.trim();
  if (!trimmed.startsWith(DIRECTIVE)) return undefined;
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed.slice(DIRECTIVE.length));
  } catch {
    return "the directive is not JSON";
  }
  if (!isMap(parsed) || typeof parsed.tag !== "string" || !TAG.test(parsed.tag) || !Array.isArray(parsed.calls)) {
    return "the directive needs a tag of 1 to 32 lower-case letters or digits and a list of calls";
  }
  if (parsed.calls.length < 1 || parsed.calls.length > MAX_CALLS) return `the directive needs 1 to ${MAX_CALLS} calls`;
  const calls: Directive["calls"] = [];
  for (const call of parsed.calls) {
    if (!isMap(call) || typeof call.name !== "string" || !TOOL_NAME.test(call.name) || !isMap(call.input)) {
      return "each call needs a tool name and an input object";
    }
    calls.push({ name: call.name, input: call.input });
  }
  return { tag: parsed.tag, calls };
}

/** One line, with what the answer guard would remove made harmless (see the header). */
function plain(value: string): string {
  return value.replace(/\s+/g, " ").replaceAll("`", "'").replaceAll("<", "‹").trim();
}

/** The tag in a tool use ID this stand-in made (`toolu_nightly_<tag>_<index>`). */
function tagOf(id: string): string | undefined {
  return /^toolu_nightly_([a-z0-9]{1,32})_\d+$/.exec(id)?.[1];
}

async function messages(req: Request, role: string): Promise<Response> {
  if (role !== ROLE) return refuse(404, "not_found_error", "no such model role");
  const tenant = req.headers.get("ai-gateway-tenant");
  if (tenant === null || !TENANT.test(tenant)) return refuse(400, "invalid_request_error", "the ai-gateway-tenant header is missing or invalid");
  const raw = await req.text();
  let body: unknown;
  try {
    body = JSON.parse(raw);
  } catch {
    return refuse(400, "invalid_request_error", "the body is not JSON");
  }
  if (!isMap(body) || !Array.isArray(body.messages)) return refuse(400, "invalid_request_error", "the body is not a Messages request");
  if (body.stream !== undefined && body.stream !== false) return refuse(400, "invalid_request_error", "this stand-in never streams");
  const history = body.messages.filter(isMap).filter((entry) => entry.role !== "system");
  const last = history.at(-1);
  if (last === undefined || last.role !== "user") return refuse(400, "invalid_request_error", "the last message is not the user's");
  const offered = (Array.isArray(body.tools) ? body.tools : []).filter(isMap).map((tool) => tool.name).filter((name): name is string => typeof name === "string");
  const bytes = Buffer.byteLength(raw);

  const results = Array.isArray(last.content) ? last.content.filter((block) => isMap(block) && block.type === "tool_result") : [];
  if (results.length > 0) {
    // The tools' names, from the assistant turn that called them.
    const names = new Map<string, string>();
    const asked = history.at(-2);
    for (const block of isMap(asked) && Array.isArray(asked.content) ? asked.content : []) {
      if (isMap(block) && block.type === "tool_use" && typeof block.id === "string" && typeof block.name === "string") names.set(block.id, block.name);
    }
    const paragraphs: string[] = [];
    const tags = new Set<string>();
    for (const [index, block] of results.entries()) {
      const result = block as Json;
      const id = typeof result.tool_use_id === "string" ? result.tool_use_id : "";
      const tool = names.get(id) ?? "unknown";
      const isError = result.is_error === true;
      const resultText = textOf(result.content) ?? "";
      const tag = tagOf(id);
      if (tag !== undefined) tags.add(tag);
      log({ event: "tool_result", tenant, tag: tag ?? null, tool_use_id: id, tool, is_error: isError, text: resultText });
      paragraphs.push(`${index + 1}. ${tool}, ${isError ? "error" : "ok"}: ${plain(resultText)}`);
    }
    log({ event: "request", tenant, role, turn: "tool_results", tag: tags.size === 1 ? [...tags][0] : null, offered, requested: [], skipped: [] });
    return message([text(`What the tools returned:\n\n${paragraphs.join("\n\n")}`)], "end_turn", bytes);
  }

  const question = textOf(last.content);
  if (question === undefined) return refuse(400, "invalid_request_error", "the last message is neither a question nor tool results");
  const directive = directiveOf(question);
  if (directive === undefined) {
    log({ event: "request", tenant, role, turn: "question", tag: null, offered, requested: [], skipped: [] });
    return message([text("The nightly stand-in answers nightly-tools directives only.")], "end_turn", bytes);
  }
  if (typeof directive === "string") {
    log({ event: "request", tenant, role, turn: "question", tag: null, offered, requested: [], skipped: [], invalid: directive });
    return message([text(`The nightly directive is not valid: ${directive}.`)], "end_turn", bytes);
  }
  const calls: Json[] = [];
  const skipped: string[] = [];
  for (const [index, call] of directive.calls.entries()) {
    if (!offered.includes(call.name)) {
      skipped.push(call.name);
      continue;
    }
    calls.push({ type: "tool_use", id: `toolu_nightly_${directive.tag}_${index}`, name: call.name, input: call.input });
  }
  log({ event: "request", tenant, role, turn: "question", tag: directive.tag, offered, requested: directive.calls.map((call) => call.name), skipped });
  if (calls.length === 0) return message([text(`None of the tools the directive names is offered: ${skipped.join(", ")}.`)], "end_turn", bytes);
  return message(calls, "tool_use", bytes);
}

function isProbe(req: Request, url: URL): boolean {
  return req.method === "GET" && (url.pathname === "/healthz" || url.pathname === "/readyz");
}

const health = Bun.serve({
  port: HEALTH_PORT,
  hostname: "0.0.0.0",
  fetch(req) {
    const url = new URL(req.url);
    if (url.pathname === LINES_PATH) return linesOf(req, url);
    return isProbe(req, url) ? new Response("ok") : new Response("not found", { status: 404 });
  },
});

const server = Bun.serve({
  port: PORT,
  hostname: "0.0.0.0",
  maxRequestBodySize: MAX_REQUEST_BYTES,
  async fetch(req) {
    const url = new URL(req.url);
    const role = MESSAGES_PATH.exec(url.pathname)?.[1];
    if (role === undefined) return refuse(404, "not_found_error", "no such route");
    if (req.method !== "POST") return refuse(405, "invalid_request_error", "only POST is served");
    return messages(req, role);
  },
});

const admin = Bun.serve({
  port: ADMIN_PORT,
  hostname: "0.0.0.0",
  fetch(req) {
    log({ event: "admin_request", method: req.method, path: new URL(req.url).pathname });
    return json(404, { type: "error", error: { type: "not_found_error", message: "the stand-in has no admin API" } });
  },
});

// The container runs this process as PID 1, which has no default SIGTERM handler.
for (const signal of ["SIGTERM", "SIGINT"] as const) {
  process.on(signal, () => {
    server.stop(true);
    admin.stop(true);
    health.stop(true);
    process.exit(0);
  });
}

log({ event: "listening", port: server.port, health_port: health.port, admin_port: admin.port });
