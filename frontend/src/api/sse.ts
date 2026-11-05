/**
 * Server-sent events over fetch, for the SPA's streams (live charts, AI answers).
 *
 * A stream can be cut without any error: Cloudflare ends a response after 125 s without data and
 * the status stays 200 (docs/architecture/deployment.md; docs/feasibility/2026-10-10-edge-poc.md).
 * So the server sends a heartbeat at least every 30 s, and the client treats HEARTBEAT_TIMEOUT_MS
 * without any data (comments included) as a lost connection, the same as an ended stream: it
 * reconnects with Last-Event-ID so the server can resume. EventSource is not used because it
 * hides comment lines and the status code.
 *
 * A stream of a tenant's data names the user and the tenant its page shows (`session.shown`, the
 * same source as the API client's, Laysi/trustiot-v2#678) on every connection, reconnects
 * included: once another tab has switched the session's tenant or signed in as someone else, the
 * cookie check refuses the stream with 409 `session_changed`, and it stops instead of serving the
 * new session's data to the old page and tells the session (`session.changed`), which reads it
 * again, as it does for a refused request.
 */

import { expectedSessionHeaders, type PinnedSession } from "./client.ts";

/** How a stream of a tenant's data follows the session (`useStreamSession()`). */
export interface StreamSession {
  /** The user and the tenant the page was mounted for, read on every connection. */
  shown: () => PinnedSession | undefined;
  /** Called when the cookie check refused the stream with 409: the session reads itself again. */
  changed(): void;
}

/** Timers the stream uses. Tests pass a manual clock, so no test waits on real time. */
export interface Clock {
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

export const systemClock: Clock = {
  setTimeout: (callback, ms) => globalThis.setTimeout(callback, ms),
  clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof globalThis.setTimeout>),
};

/** What the stream reads of a response; `fetch`'s Response has it. */
export interface StreamResponse {
  status: number;
  headers: { get(name: string): string | null };
  body: ReadableStream<Uint8Array> | null;
}
export type StreamFetch = (url: string, init: RequestInit) => Promise<StreamResponse>;

export interface ServerSentEvent {
  /** The `event:` field; "message" when the event has none. */
  type: string;
  data: string;
  /** The last event ID at this event (the stream's, when the event has no `id:` of its own). */
  id: string;
}

export type StreamState =
  | "connecting"
  | "open"
  | "reconnecting"
  | "unauthenticated"
  | "forbidden"
  | "session_changed"
  | "refused"
  | "closed";

/** The server's heartbeat interval (docs/architecture/deployment.md: at most 60 s; 30 s was tested). */
export const SERVER_HEARTBEAT_MS = 30_000;
/** Silence after which the connection counts as lost: two missed heartbeats, well under 125 s. */
export const HEARTBEAT_TIMEOUT_MS = 75_000;
export const MIN_BACKOFF_MS = 1_000;
export const MAX_BACKOFF_MS = 30_000;
/**
 * How long a connection must stay open to count as healthy without moving the stream forward
 * (`progressed`). As long as the longest wait: a server that keeps closing streams that delivered
 * nothing is asked again at most as often as the backoff's ceiling allows.
 */
export const HEALTHY_CONNECTION_MS = MAX_BACKOFF_MS;

/**
 * The event stream format (WHATWG HTML, "Server-sent events", parsing an event stream). Feed it
 * decoded text in any chunks; it returns the events that completed.
 */
export class EventStreamParser {
  /**
   * The id of the last event dispatched (the spec's last event ID string). An `id:` line takes
   * effect only when its event is dispatched, so a connection cut inside an event never resumes
   * past what was handed over. Stricter than the spec, an `id:` in a frame without data (no event)
   * does not take effect either: the resume position only ever moves with an event.
   */
  lastEventId: string;
  /** The `retry:` the server asked for, in milliseconds. */
  retry: number | undefined;
  /** The spec's last event ID buffer: the latest `id:` line, committed when an event is dispatched. */
  private pendingId: string;
  private buffer = "";
  private data = "";
  private type = "";
  private started = false;
  private skipLineFeed = false;

  constructor(lastEventId = "") {
    this.lastEventId = lastEventId;
    this.pendingId = lastEventId;
  }

  push(text: string): ServerSentEvent[] {
    let chunk = text;
    if (!this.started && chunk.length > 0) {
      this.started = true;
      if (chunk.startsWith("﻿")) chunk = chunk.slice(1);
    }
    if (this.skipLineFeed && chunk.startsWith("\n")) {
      chunk = chunk.slice(1);
    }
    this.skipLineFeed = false;
    this.buffer += chunk;
    const events: ServerSentEvent[] = [];
    for (;;) {
      const end = this.buffer.search(/[\r\n]/);
      if (end < 0) break;
      const line = this.buffer.slice(0, end);
      let next = end + 1;
      if (this.buffer[end] === "\r") {
        if (end + 1 === this.buffer.length) {
          // A CR at the end of the chunk may be the first half of CRLF.
          this.skipLineFeed = true;
        } else if (this.buffer[end + 1] === "\n") {
          next += 1;
        }
      }
      this.buffer = this.buffer.slice(next);
      const event = this.line(line);
      if (event) events.push(event);
    }
    return events;
  }

  private line(line: string): ServerSentEvent | undefined {
    if (line === "") {
      return this.dispatch();
    }
    if (line.startsWith(":")) {
      return undefined;
    }
    const colon = line.indexOf(":");
    const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    switch (field) {
      case "event":
        this.type = value;
        break;
      case "data":
        this.data += `${value}\n`;
        break;
      case "id":
        if (!value.includes("\0")) this.pendingId = value;
        break;
      case "retry":
        if (/^[0-9]+$/.test(value)) this.retry = Number(value);
        break;
      default:
        break;
    }
    return undefined;
  }

  private dispatch(): ServerSentEvent | undefined {
    const data = this.data;
    const type = this.type;
    this.data = "";
    this.type = "";
    if (data === "") {
      return undefined;
    }
    this.lastEventId = this.pendingId;
    return { type: type || "message", data: data.slice(0, -1), id: this.lastEventId };
  }
}

export interface StreamOptions {
  onEvent(event: ServerSentEvent): void;
  onState?(state: StreamState): void;
  heartbeatTimeoutMs?: number;
  fetch?: StreamFetch;
  clock?: Clock;
  /** In [0, 1); the jitter of the reconnect delay. */
  random?: () => number;
  /** Closes the stream when aborted. */
  signal?: AbortSignal;
  /**
   * The session's side of the stream (`useStreamSession()` from the session provider). Required,
   * so no stream of a tenant's data leaves it out by accident; `null` only for a stream that is
   * not about a tenant. A refusal (409) calls `changed` and stops the stream in the state
   * `session_changed`.
   */
  session: StreamSession | null;
  /**
   * Where the stream starts: sent as `Last-Event-ID` on the first connection (and on later ones
   * until an event sets another). Empty or absent starts from the beginning.
   */
  lastEventId?: string;
  /**
   * Statuses, besides 401, 403 and 409, that reconnecting cannot fix for this stream (a stream
   * that is gone, a resume position the server refuses): the stream stops in the state `refused`
   * and calls `onRefused` with the status, instead of retrying with backoff.
   */
  stopOn?: readonly number[];
  onRefused?(status: number): void;
  /**
   * Whether `event`, just handed to `onEvent`, moved the stream forward; by default every event
   * but a `heartbeat` does. A connection that ends without moving it forward, and was open less
   * than HEALTHY_CONNECTION_MS, is a failed attempt, so the wait before the next one keeps
   * growing: a server that closes each stream right after its first heartbeat is not asked again
   * every second. One that moved it forward starts the waits small again.
   */
  progressed?(event: ServerSentEvent): boolean;
}

export interface EventStream {
  close(): void;
}

/** The delay before reconnect attempt `attempt` (1-based): doubling from 1 s to 30 s, half of it jittered. */
export function backoff(attempt: number, random: () => number, serverRetry?: number): number {
  const ceiling = Math.min(MAX_BACKOFF_MS, MIN_BACKOFF_MS * 2 ** Math.max(0, attempt - 1));
  const jittered = ceiling / 2 + (random() * ceiling) / 2;
  return Math.min(MAX_BACKOFF_MS, Math.max(serverRetry ?? 0, jittered));
}

/** Rejects when `signal` aborts, also if `promise` itself ignores the signal. */
function abortable<T>(promise: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    if (signal.aborted) {
      reject(signal.reason);
      return;
    }
    signal.addEventListener("abort", () => reject(signal.reason), { once: true });
    promise.then(resolve, reject);
  });
}

type StopState = "unauthenticated" | "forbidden" | "session_changed";

type Attempt =
  | { kind: "retry"; healthy: boolean }
  | { kind: "stop"; state: StopState }
  | { kind: "refused"; status: number };

/** The statuses that reconnecting cannot fix, and the state the stream stops in. */
const STOP_STATES: ReadonlyMap<number, StopState> = new Map([
  [401, "unauthenticated"],
  [403, "forbidden"],
  [409, "session_changed"],
]);

/**
 * Opens `url` (same origin, with the session cookie) and keeps it open: reconnects after a lost
 * or ended connection, after a network error and after a 5xx, with backoff; stops on 401 and 403,
 * which reconnecting cannot fix, and on 409 (`session_changed`), after telling the session; and
 * on the statuses of `stopOn`, in the state `refused`.
 */
export function openEventStream(url: string, options: StreamOptions): EventStream {
  const fetchStream: StreamFetch = options.fetch ?? ((target, init) => fetch(target, init));
  const clock = options.clock ?? systemClock;
  const random = options.random ?? Math.random;
  const timeoutMs = options.heartbeatTimeoutMs ?? HEARTBEAT_TIMEOUT_MS;
  const parser = { lastEventId: options.lastEventId ?? "", retry: undefined as number | undefined };
  const stopOn = new Set(options.stopOn ?? []);
  const progressed = options.progressed ?? ((event: ServerSentEvent) => event.type !== "heartbeat");

  let closed = false;
  let connection: AbortController | null = null;
  let reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  let sleeping: { handle: unknown; wake: () => void } | null = null;

  const setState = (state: StreamState) => options.onState?.(state);

  function close() {
    if (closed) return;
    closed = true;
    connection?.abort();
    reader?.cancel().catch(() => {});
    if (sleeping) {
      clock.clearTimeout(sleeping.handle);
      sleeping.wake();
    }
    setState("closed");
  }

  function sleep(ms: number): Promise<void> {
    return new Promise((resolve) => {
      const wake = () => {
        sleeping = null;
        resolve();
      };
      sleeping = { handle: clock.setTimeout(wake, ms), wake };
    });
  }

  async function connect(): Promise<Attempt> {
    const controller = new AbortController();
    connection = controller;
    // Whether the connection moved the stream forward or stayed open long enough to count as healthy.
    let healthy = false;
    let watchdog: unknown;
    let lasted: unknown;
    const arm = () => {
      if (watchdog !== undefined) clock.clearTimeout(watchdog);
      watchdog = clock.setTimeout(() => {
        controller.abort();
        reader?.cancel().catch(() => {});
      }, timeoutMs);
    };
    arm();
    try {
      const headers: Record<string, string> = { Accept: "text/event-stream" };
      if (parser.lastEventId !== "") headers["Last-Event-ID"] = parser.lastEventId;
      // A stream is about a tenant's data: it names the user and the tenant (no schema path).
      for (const [name, value] of expectedSessionHeaders(options.session?.shown(), "")) headers[name] = value;
      const response = await abortable(
        fetchStream(url, { headers, credentials: "same-origin", cache: "no-store", signal: controller.signal }),
        controller.signal,
      );
      const stop = STOP_STATES.get(response.status);
      if (stop !== undefined) {
        // The error body is not read: release the connection before stopping.
        await response.body?.cancel().catch(() => {});
        if (stop === "session_changed") options.session?.changed();
        return { kind: "stop", state: stop };
      }
      if (stopOn.has(response.status)) {
        await response.body?.cancel().catch(() => {});
        return { kind: "refused", status: response.status };
      }
      const type = response.headers.get("content-type") ?? "";
      if (response.status !== 200 || response.body === null || !type.toLowerCase().startsWith("text/event-stream")) {
        await response.body?.cancel().catch(() => {});
        return { kind: "retry", healthy };
      }
      setState("open");
      lasted = clock.setTimeout(() => {
        healthy = true;
      }, HEALTHY_CONNECTION_MS);
      const events = new EventStreamParser(parser.lastEventId);
      const decoder = new TextDecoder();
      reader = response.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done || closed) break;
        arm();
        for (const event of events.push(decoder.decode(value, { stream: true }))) {
          // A handler may close the stream; nothing after that reaches it.
          if (closed) break;
          try {
            options.onEvent(event);
            if (progressed(event)) healthy = true;
          } catch (error) {
            console.error("event stream handler failed", error);
          }
        }
        parser.lastEventId = events.lastEventId;
        parser.retry = events.retry ?? parser.retry;
      }
    } catch {
      // A network error, or the watchdog aborted a silent connection: reconnect.
    } finally {
      clock.clearTimeout(watchdog);
      if (lasted !== undefined) clock.clearTimeout(lasted);
      reader = null;
      connection = null;
    }
    return { kind: "retry", healthy };
  }

  async function run() {
    let failures = 0;
    setState("connecting");
    while (!closed) {
      const attempt = await connect();
      if (closed) return;
      if (attempt.kind === "stop") {
        closed = true;
        setState(attempt.state);
        return;
      }
      if (attempt.kind === "refused") {
        closed = true;
        setState("refused");
        options.onRefused?.(attempt.status);
        return;
      }
      // A healthy connection starts the waits small again; any other attempt makes the next wait
      // longer, up to MAX_BACKOFF_MS.
      failures = attempt.healthy ? 1 : failures + 1;
      setState("reconnecting");
      await sleep(backoff(failures, random, parser.retry));
    }
  }

  if (options.signal?.aborted) {
    closed = true;
    setState("closed");
  } else {
    options.signal?.addEventListener("abort", close, { once: true });
    void run();
  }
  return { close };
}
