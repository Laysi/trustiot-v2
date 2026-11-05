import type { components } from "../api/schema.gen.ts";
import type { HttpPreviewDeclaration } from "../ui-preview/contract.ts";
import { FakeConversationServer, seedPreviewConversations } from "./fake-conversations.ts";
import { fakeError } from "./fake-dashboards.ts";
import { fakeUuid } from "./fake-devices.ts";

/**
 * The conversation endpoints plus the question endpoints of Laysi/trustiot-v2#646
 * (services/assistant/README.md 「提問」), held in memory and answered at the HTTP level like
 * `FakeConversationServer`, which it extends; the pages go through the real client:
 *
 * - `POST /conversations` starts an empty conversation without a title (201, `Location`).
 * - `POST .../questions` (`{ question }`) records a run and answers 202 with it (`Location`). A
 *   conversation with a running question is 409 `conversation.question_running`; one that is not
 *   the caller's or was deleted 404; a question that is empty after trimming or over `maxChars`
 *   characters 422 (`/question`, `question.text_invalid`).
 * - `GET .../conversations/{id}` gives the messages and `latest_run`, the newest run (or `null`).
 * - A run stays `running` until the test (or, with `autoAnswerMs`, a timer) finishes it:
 *   `answer` stores the round (question and answer) and sets the run `answered`, `fail` sets it
 *   `failed` with an `AgentError` kind and its `message_key` (`question.<kind>`), `interrupt`
 *   sets it `interrupted`; none of the last two stores the round.
 * - `POST .../runs/{id}/cancel` answers 202 with the run, still `running` with
 *   `stop_requested_at` until it has stopped (`settleStop`, or `stopSettleMs` after the request
 *   in the preview), then `cancelled` with `question.cancelled` and no round stored; a run that
 *   ended is 409 `question.run_ended`; a run of another conversation 404.
 * - Deleting a conversation with a running question stops it (`cancelled`) and deletes it.
 * - `GET .../runs/{id}/stream` is the answer stream (Laysi/trustiot-v2#147, 「回答的串流」):
 *   `retry` and a `heartbeat` at once, then `text` events (`{offset, text}`, `id` the end
 *   offset, at most 4,096 UTF-16 code units each) from `Last-Event-ID` on, and `end` with the
 *   run once it ended, after which the response closes. While the run runs, `write` adds a
 *   paragraph to what went out of its answer (the service's partial answer: finished
 *   paragraphs, trimmed, each after the first with its blank line in front), and `answer` must
 *   start with it, as the stored answer does; a run that ends without an answer sends only `end`.
 *   A position that is not 1 to 10 digits or past what went out is 400 `stream.resume_invalid`;
 *   a run of another conversation, or a deleted conversation, 404. `cutStreams` closes every
 *   open stream without `end` (status 200, as a proxy cuts it), `stallStreams` stops writing to
 *   them and keeps them open, `heartbeat` sends a heartbeat on each, and `streamOpens` records
 *   every opening with its `Last-Event-ID`.
 *
 * Every request is recorded (`requests`, the question endpoints included). `failNextOn` answers
 * the next request to one of the new endpoints with an error; `holdAsks` keeps asks waiting until
 * `releaseAsks`. With `scripted` (the UI preview), words in a question choose what happens:
 * `!fail` ends in a model error, `!slow` never answers until it is stopped, `!limit` is refused
 * with 429 and `!denied` with 403.
 */

type Schemas = components["schemas"];
type RunStatus = Schemas["ConversationRunStatus"];
type NewEndpoint = "create" | "ask" | "stop" | "stream";

export interface FakeRun {
  id: string;
  conversationId: string;
  text: string;
  status: RunStatus;
  result?: string;
  messageKey?: string;
  startedAt: string;
  endedAt?: string;
  stopRequestedAt?: string;
  /** What went out of the answer while the run runs (the service's partial answer); none when absent. */
  partial?: string;
  /** The stored answer, once the run answered. */
  answer?: string;
}

/** One open answer stream. */
interface OpenStream {
  runId: string;
  /** Where the next `text` event starts. */
  position: number;
  stalled: boolean;
  controller: ReadableStreamDefaultController<Uint8Array>;
}

const QUESTIONS_PATH = /^\/api\/assistant\/v1\/conversations\/([^/]+)\/questions$/;
const STOP_PATH = /^\/api\/assistant\/v1\/conversations\/([^/]+)\/runs\/([^/]+)\/cancel$/;
const STREAM_PATH = /^\/api\/assistant\/v1\/conversations\/([^/]+)\/runs\/([^/]+)\/stream$/;
/** The most UTF-16 code units one `text` event carries (the service's `TEXT_CHUNK_UNITS`). */
const TEXT_CHUNK_UNITS = 4096;
const encoder = new TextEncoder();
const CONVERSATION_PATH = /^\/api\/assistant\/v1\/conversations\/([^/]+)$/;
const CONVERSATIONS_PATH = "/api/assistant/v1/conversations";
const DEADLINE_MS = 15 * 60 * 1000;

function json(status: number, body: unknown, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", ...headers } });
}

/** The preview's stand-in for the model's answer: Markdown, so the answer's rules show. */
function demoAnswer(question: string): string {
  return [
    `This is a fictional answer for the UI preview. You asked: "${question}".`,
    "",
    "| Device | Energy this week |",
    "| - | -: |",
    "| Cooling tower 2 | 412 kWh |",
    "| Air compressor 1 | 128 kWh |",
  ].join("\n");
}

export class FakeQuestionServer extends FakeConversationServer {
  readonly runs = new Map<string, FakeRun>();
  /** The longest question the fake accepts, in characters. */
  maxChars = 16000;
  /** Finish a run by itself after this many milliseconds (the preview); `null` leaves it to the caller. */
  autoAnswerMs: number | null = null;
  /** Settle a stop by itself this many milliseconds after the request (the preview); `null` leaves it to the caller. */
  stopSettleMs: number | null = null;
  /** Let words in a question choose the outcome (the preview). */
  scripted = false;
  private readonly newFailures = new Map<NewEndpoint, Response[]>();
  private askGate: (() => void)[] | null = null;
  private readonly timers = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly streams = new Set<OpenStream>();
  /** Every opening of an answer stream, with the `Last-Event-ID` it sent (`null` for none). */
  readonly streamOpens: { runId: string; lastEventId: string | null }[] = [];
  /** The abort signal of every answer stream request, in the order of `streamOpens`. */
  readonly streamSignals: AbortSignal[] = [];

  /** Adds `text` to what went out of the running question's answer, and sends it on each open stream. */
  write(conversationId: string, text: string): void {
    const run = this.runningRun(conversationId);
    if (run === undefined) return;
    run.partial = (run.partial ?? "") + text;
    for (const stream of this.streams) if (stream.runId === run.id) this.sendText(stream, run.partial);
  }

  /** Closes every open answer stream without `end`, with the status 200 it had: a proxy cut it. */
  cutStreams(): void {
    for (const stream of [...this.streams]) this.closeStream(stream);
  }

  /** Stops writing to every open answer stream and keeps it open: a proxy stopped forwarding it. */
  stallStreams(): void {
    for (const stream of this.streams) stream.stalled = true;
  }

  /** Sends a heartbeat on every open answer stream that is not stalled. */
  heartbeat(): void {
    for (const stream of this.streams) if (!stream.stalled) this.send(stream, "event: heartbeat\ndata: {}\n\n");
  }

  /** How many answer streams are open now. */
  get openStreams(): number {
    return this.streams.size;
  }

  /** The next request to `endpoint` gets `response`. */
  failNextOn(endpoint: NewEndpoint, response: Response): void {
    this.newFailures.set(endpoint, [...(this.newFailures.get(endpoint) ?? []), response]);
  }

  /** Asks wait from now on until `releaseAsks`. */
  holdAsks(): void {
    this.askGate ??= [];
  }

  /** Lets every waiting ask go on, and stops holding new ones. */
  releaseAsks(): void {
    const waiting = this.askGate ?? [];
    this.askGate = null;
    for (const resume of waiting) resume();
  }

  /** The conversation's newest run. */
  latestRun(conversationId: string): FakeRun | undefined {
    return [...this.runs.values()].reverse().find((run) => run.conversationId === conversationId);
  }

  /** Ends the conversation's running question with an answer, which is stored with the question. */
  answer(conversationId: string, text?: string): void {
    const run = this.runningRun(conversationId);
    const conversation = this.conversations.get(conversationId);
    if (run === undefined || conversation === undefined) return;
    if (run.stopRequestedAt !== undefined) {
      this.settleStop(conversationId);
      return;
    }
    const answer = text ?? demoAnswer(run.text);
    if (!answer.startsWith(run.partial ?? "")) {
      throw new Error("the stored answer starts with what went out of it, as the service's does");
    }
    run.answer = answer;
    conversation.messages.push({ kind: "question", text: run.text }, { kind: "answer", text: answer });
    this.finish(run, "answered");
    conversation.last_activity_at = this.now();
    if (conversation.title === undefined) {
      conversation.title = [...run.text].slice(0, 40).join("");
      conversation.title_source = "provisional";
    }
  }

  /** Ends the conversation's running question as failed with the `AgentError` kind; nothing is stored. */
  fail(conversationId: string, kind = "model_unavailable"): void {
    const run = this.runningRun(conversationId);
    if (run === undefined) return;
    run.result = kind;
    run.messageKey = `question.${kind}`;
    this.finish(run, "failed");
  }

  /** Ends the conversation's running question as interrupted (the service restarted). */
  interrupt(conversationId: string): void {
    const run = this.runningRun(conversationId);
    if (run === undefined) return;
    run.messageKey = "question.interrupted";
    this.finish(run, "interrupted");
  }

  /** The running question stops, as it does a little after a stop request: `cancelled`, nothing stored. */
  settleStop(conversationId: string): void {
    const run = this.runningRun(conversationId);
    if (run === undefined) return;
    run.messageKey = "question.cancelled";
    this.finish(run, "cancelled");
  }

  override async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (request.method === "POST" && path === CONVERSATIONS_PATH) {
      return this.record(request, "create", () => this.create());
    }
    const questions = QUESTIONS_PATH.exec(path);
    if (request.method === "POST" && questions) {
      const id = decodeURIComponent(questions[1] as string);
      return this.record(request, "ask", (body) => this.ask(id, body));
    }
    const stream = STREAM_PATH.exec(path);
    if (request.method === "GET" && stream) {
      const [id, runId] = [decodeURIComponent(stream[1] as string), decodeURIComponent(stream[2] as string)];
      this.requests.push({ method: request.method, url: new URL(request.url), body: undefined });
      return this.newFailures.get("stream")?.shift() ?? this.openStream(id, runId, request);
    }
    const stop = STOP_PATH.exec(path);
    if (request.method === "POST" && stop) {
      const [id, runId] = [decodeURIComponent(stop[1] as string), decodeURIComponent(stop[2] as string)];
      return this.record(request, "stop", () => this.stop(id, runId));
    }
    const conversation = CONVERSATION_PATH.exec(path);
    const id = conversation ? decodeURIComponent(conversation[1] as string) : "";
    const before = request.method === "DELETE" ? this.runningRun(id) : undefined;
    const response = await super.fetch(request);
    if (before !== undefined && response.status === 204) {
      // Deleting a conversation with a running question stops that question.
      before.stopRequestedAt ??= this.now();
      before.messageKey = "question.cancelled";
      this.finish(before, "cancelled");
    }
    if (conversation && request.method === "GET" && response.ok) {
      const body = (await response.json()) as Record<string, unknown>;
      const run = this.latestRun(id);
      return json(200, { ...body, latest_run: run ? this.runBody(run) : null });
    }
    return response;
  }

  private runBody(run: FakeRun): Schemas["ConversationRun"] {
    return {
      id: run.id,
      status: run.status,
      result: run.result ?? null,
      message_key: run.messageKey ?? null,
      started_at: run.startedAt,
      deadline_at: new Date(Date.parse(run.startedAt) + DEADLINE_MS).toISOString(),
      ended_at: run.endedAt ?? null,
      stop_requested_at: run.stopRequestedAt ?? null,
    };
  }

  private visibleConversation(id: string): boolean {
    const conversation = this.conversations.get(id);
    return conversation !== undefined && conversation.deleted_at === undefined;
  }

  private runningRun(conversationId: string): FakeRun | undefined {
    return [...this.runs.values()].find((run) => run.conversationId === conversationId && run.status === "running");
  }

  private finish(run: FakeRun, status: RunStatus): void {
    run.status = status;
    run.endedAt = this.now();
    clearTimeout(this.timers.get(run.id));
    this.timers.delete(run.id);
    for (const stream of [...this.streams]) if (stream.runId === run.id && !stream.stalled) this.sendEnd(stream, run);
  }

  private openStream(conversationId: string, runId: string, request: Request): Response {
    const run = this.runs.get(runId);
    if (run === undefined || run.conversationId !== conversationId || !this.visibleConversation(conversationId)) {
      return fakeError(404, "not_found", "error.not_found");
    }
    const header = request.headers.get("last-event-id");
    this.streamOpens.push({ runId, lastEventId: header });
    this.streamSignals.push(request.signal);
    const position = header === null || header === "" ? 0 : /^[0-9]{1,10}$/.test(header) ? Number(header) : -1;
    const available = (run.status === "running" ? (run.partial ?? "") : (run.answer ?? "")).length;
    const unanswered = run.status !== "running" && run.status !== "answered";
    if (position < 0 || (!unanswered && position > available)) {
      return fakeError(400, "invalid_request", "stream.resume_invalid");
    }
    let open: OpenStream | undefined;
    const body = new ReadableStream<Uint8Array>({
      start: (controller) => {
        open = { runId, position, stalled: false, controller };
        this.streams.add(open);
        this.send(open, "retry: 1000\nevent: heartbeat\ndata: {}\n\n");
        if (run.status === "running") this.sendText(open, run.partial ?? "");
        else this.sendEnd(open, run);
      },
      cancel: () => {
        if (open) this.streams.delete(open);
      },
    });
    return new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
  }

  private send(stream: OpenStream, frame: string): void {
    try {
      stream.controller.enqueue(encoder.encode(frame));
    } catch {
      // The reader went away; its cancel removes the stream.
      this.streams.delete(stream);
    }
  }

  /** `text` events from the stream's position to the end of `text`, in pieces that split no character. */
  private sendText(stream: OpenStream, text: string): void {
    if (stream.stalled) return;
    while (stream.position < text.length) {
      let end = Math.min(stream.position + TEXT_CHUNK_UNITS, text.length);
      const last = text.charCodeAt(end - 1);
      if (end < text.length && last >= 0xd800 && last <= 0xdbff) end -= 1;
      const piece = { offset: stream.position, text: text.slice(stream.position, end) };
      this.send(stream, `event: text\nid: ${end}\ndata: ${JSON.stringify(piece)}\n\n`);
      stream.position = end;
    }
  }

  /** The rest of an answered run's answer, then `end` with the run; the stream closes. */
  private sendEnd(stream: OpenStream, run: FakeRun): void {
    if (run.status === "answered") this.sendText(stream, run.answer ?? "");
    this.send(stream, `event: end\ndata: ${JSON.stringify(this.runBody(run))}\n\n`);
    this.closeStream(stream);
  }

  private closeStream(stream: OpenStream): void {
    this.streams.delete(stream);
    try {
      stream.controller.close();
    } catch {
      // The reader cancelled it already.
    }
  }

  private async record(
    request: Request,
    endpoint: NewEndpoint,
    handle: (body: unknown) => Response,
  ): Promise<Response> {
    const text = await request.text();
    const body: unknown = text === "" ? undefined : JSON.parse(text);
    this.requests.push({ method: request.method, url: new URL(request.url), body });
    if (endpoint === "ask" && this.askGate !== null) {
      const gate = this.askGate;
      await new Promise<void>((resume, reject) => {
        gate.push(resume);
        request.signal.addEventListener("abort", () => reject(request.signal.reason), { once: true });
      });
    }
    const failure = this.newFailures.get(endpoint)?.shift();
    return failure ?? handle(body);
  }

  private create(): Response {
    const now = this.now();
    const id = fakeUuid();
    this.conversations.set(id, { id, created_at: now, last_activity_at: now, messages: [] });
    return json(
      201,
      { id, title: null, title_source: null, created_at: now, last_activity_at: now, archived_at: null },
      { location: `${CONVERSATIONS_PATH}/${id}` },
    );
  }

  private ask(conversationId: string, body: unknown): Response {
    if (!this.visibleConversation(conversationId)) return fakeError(404, "not_found", "error.not_found");
    const question = (body as { question?: unknown } | undefined)?.question;
    if (typeof question !== "string") return fakeError(400, "invalid_request", "conversation.body_invalid");
    const text = question.trim();
    if (text === "" || [...text].length > this.maxChars) {
      return json(422, {
        code: "validation_failed",
        message_key: "error.validation_failed",
        field_errors: [{ field: "/question", message_key: "question.text_invalid", details: { max: this.maxChars } }],
      });
    }
    if (this.scripted && text.includes("!limit")) {
      return fakeError(429, "rate_limited", "question.running_limited", { max: 3 });
    }
    if (this.scripted && text.includes("!denied"))
      return fakeError(403, "permission_denied", "error.permission_denied");
    if (this.runningRun(conversationId) !== undefined) {
      return fakeError(409, "conflict", "conversation.question_running");
    }
    const run: FakeRun = {
      id: fakeUuid(),
      conversationId,
      text,
      status: "running",
      startedAt: this.now(),
    };
    this.runs.set(run.id, run);
    if (this.autoAnswerMs !== null && this.scripted) {
      // The preview streams the example answer's first paragraph halfway, as the service releases
      // one (finished, trimmed); a `!fail` question then drops it, a `!slow` one keeps it until it
      // is stopped.
      const first = demoAnswer(text).split("\n\n")[0] ?? "";
      setTimeout(() => this.write(conversationId, first), this.autoAnswerMs / 2);
    }
    if (this.autoAnswerMs !== null && !(this.scripted && text.includes("!slow"))) {
      this.timers.set(
        run.id,
        setTimeout(() => {
          if (this.scripted && text.includes("!fail")) this.fail(conversationId);
          else this.answer(conversationId);
        }, this.autoAnswerMs),
      );
    }
    return json(202, this.runBody(run), { location: `${CONVERSATIONS_PATH}/${conversationId}` });
  }

  private stop(conversationId: string, runId: string): Response {
    const run = this.runs.get(runId);
    if (run === undefined || run.conversationId !== conversationId || !this.visibleConversation(conversationId)) {
      return fakeError(404, "not_found", "error.not_found");
    }
    if (run.status !== "running") return fakeError(409, "conflict", "question.run_ended");
    run.stopRequestedAt ??= this.now();
    if (this.stopSettleMs !== null) {
      clearTimeout(this.timers.get(run.id));
      this.timers.set(
        run.id,
        setTimeout(() => this.settleStop(conversationId), this.stopSettleMs),
      );
    }
    return json(202, this.runBody(run));
  }
}

/** The preview's server: the conversations of `seedPreviewConversations`, answered by the scripted rules. */
function previewServer(seed: "normal" | "empty"): FakeQuestionServer {
  const server = new FakeQuestionServer();
  server.autoAnswerMs = 2500;
  server.stopSettleMs = 600;
  server.scripted = true;
  seedPreviewConversations(server, seed);
  return server;
}

/**
 * The preview's entry for every conversation path (the only one: fake-conversations.ts has none,
 * a path prefix has one owner): the conversation endpoints with questions on top. In the question
 * box, `!fail`, `!slow`, `!limit` and `!denied` choose a model error, a question that runs until it
 * is stopped, the running-questions limit and a refusal; any other question is answered after a few
 * seconds.
 */
export const preview: HttpPreviewDeclaration = {
  name: "conversations",
  http: {
    prefixes: ["/api/assistant/v1/conversations", "/api/assistant/v1/deleted-conversations"],
    create: (seed) => {
      const server = previewServer(seed);
      return { fetch: (request) => server.fetch(request) };
    },
  },
};
