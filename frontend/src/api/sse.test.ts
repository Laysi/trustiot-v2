import { describe, expect, test } from "bun:test";

import { EXPECTED_NO_TENANT, EXPECTED_TENANT_HEADER, EXPECTED_USER_HEADER, type ShownSession } from "./client.ts";
import {
  backoff,
  type Clock,
  EventStreamParser,
  HEARTBEAT_TIMEOUT_MS,
  MAX_BACKOFF_MS,
  openEventStream,
  type ServerSentEvent,
  type StreamFetch,
  type StreamResponse,
  type StreamState,
} from "./sse.ts";

describe("EventStreamParser", () => {
  test("events, multi-line data, event types and ids", () => {
    const parser = new EventStreamParser();
    expect(parser.push("data: one\n\nevent: point\nid: 7\ndata: a\ndata: b\n\n")).toEqual([
      { type: "message", data: "one", id: "" },
      { type: "point", data: "a\nb", id: "7" },
    ]);
    expect(parser.lastEventId).toBe("7");
  });

  test("comments (the heartbeat) are not events", () => {
    expect(new EventStreamParser().push(": heartbeat\n\n:\n\n")).toEqual([]);
  });

  test("chunks split anywhere, CRLF and CR line ends, and a leading BOM", () => {
    const parser = new EventStreamParser();
    const text = "\uFEFFdata: x\r\n\r\ndata: y\r\rdata:z\n\n";
    const events: ServerSentEvent[] = [];
    for (const character of text) events.push(...parser.push(character));
    expect(events.map((event) => event.data)).toEqual(["x", "y", "z"]);
  });

  test("a CR at the end of one chunk and LF at the start of the next is one line end", () => {
    const parser = new EventStreamParser();
    expect(parser.push("data: a\r")).toEqual([]);
    // The LF completes the CRLF; the next CR is the blank line that ends the event.
    expect(parser.push("\n\r")).toEqual([{ type: "message", data: "a", id: "" }]);
    // And this LF belongs to that CR, so it is not another (empty) line.
    expect(parser.push("\ndata: b\n\n")).toEqual([{ type: "message", data: "b", id: "" }]);
  });

  test("retry, ids with NUL, unknown fields and fields without a value", () => {
    const parser = new EventStreamParser("3");
    expect(parser.push("retry: 5000\nretry: soon\nid: a\0b\nfoo: bar\ndata\n\n")).toEqual([
      { type: "message", data: "", id: "3" },
    ]);
    expect(parser.retry).toBe(5000);
  });

  test("an event without data is not dispatched, and its type does not leak into the next", () => {
    expect(new EventStreamParser().push("event: x\n\ndata: d\n\n")).toEqual([{ type: "message", data: "d", id: "" }]);
  });
});

describe("backoff", () => {
  test("doubles from 1 s and stops at 30 s", () => {
    const top = () => 1;
    expect([1, 2, 3, 4, 5, 6, 7, 20].map((attempt) => backoff(attempt, top))).toEqual([
      1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000,
    ]);
  });

  test("jitter takes at most half off", () => {
    expect(backoff(1, () => 0)).toBe(500);
    expect(backoff(6, () => 0)).toBe(15_000);
  });

  test("the server's retry is a lower bound, within the ceiling", () => {
    expect(backoff(1, () => 1, 5_000)).toBe(5_000);
    expect(backoff(1, () => 1, 120_000)).toBe(MAX_BACKOFF_MS);
  });
});

/** Timers that move only when the test says so. */
class ManualClock implements Clock {
  now = 0;
  private next = 1;
  private readonly timers = new Map<number, { at: number; callback: () => void }>();

  setTimeout(callback: () => void, ms: number): unknown {
    const id = this.next++;
    this.timers.set(id, { at: this.now + ms, callback });
    return id;
  }

  clearTimeout(handle: unknown): void {
    this.timers.delete(handle as number);
  }

  /** Moves time forward, firing every timer that falls due on the way, in order. */
  async advance(ms: number): Promise<void> {
    const target = this.now + ms;
    for (;;) {
      await settle();
      const due = [...this.timers.entries()]
        .filter(([, timer]) => timer.at <= target)
        .sort((a, b) => a[1].at - b[1].at)[0];
      if (due === undefined) break;
      this.timers.delete(due[0]);
      this.now = due[1].at;
      due[1].callback();
    }
    this.now = target;
    await settle();
  }
}

/** Lets pending promise callbacks and stream reads run; no real time passes. */
async function settle(): Promise<void> {
  for (let turn = 0; turn < 5; turn += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
}

/** Waits, by event loop turns rather than time, until `condition` holds. */
async function until(what: string, condition: () => boolean): Promise<void> {
  for (let turn = 0; turn < 1_000; turn += 1) {
    if (condition()) return;
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
  throw new Error(`timed out waiting for ${what}`);
}

function fakeStream() {
  let controller: ReadableStreamDefaultController<Uint8Array> | undefined;
  let cancelled = false;
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
    },
    cancel() {
      cancelled = true;
    },
  });
  const encoder = new TextEncoder();
  return {
    body,
    get cancelled() {
      return cancelled;
    },
    send(text: string) {
      controller?.enqueue(encoder.encode(text));
    },
    end() {
      controller?.close();
    },
  };
}

function eventStream(body: ReadableStream<Uint8Array>): StreamResponse {
  return { status: 200, headers: new Headers({ "content-type": "text/event-stream; charset=utf-8" }), body };
}

function status(code: number): StreamResponse {
  return { status: code, headers: new Headers({ "content-type": "text/plain" }), body: null };
}

/** A fetch that answers with the given responses in order, then never answers. */
function fakeFetch(responses: Array<() => Promise<StreamResponse>>) {
  const calls: Array<{ url: string; headers: Record<string, string>; credentials: RequestCredentials | undefined }> =
    [];
  const fetch: StreamFetch = (url, init) => {
    calls.push({ url, headers: { ...(init.headers as Record<string, string>) }, credentials: init.credentials });
    const next = responses.shift();
    return next ? next() : new Promise<StreamResponse>(() => {});
  };
  return { fetch, calls };
}

function start(fetch: StreamFetch, clock: ManualClock) {
  const events: ServerSentEvent[] = [];
  const states: StreamState[] = [];
  const stream = openEventStream("/api/x/v1/live", {
    fetch,
    clock,
    random: () => 1,
    onEvent: (event) => events.push(event),
    onState: (state) => states.push(state),
    session: null,
  });
  return { stream, events, states };
}

describe("openEventStream", () => {
  test("a heartbeat keeps a quiet stream open", async () => {
    const clock = new ManualClock();
    const live = fakeStream();
    const { fetch, calls } = fakeFetch([async () => eventStream(live.body)]);
    const { states, stream } = start(fetch, clock);
    await until("the stream to open", () => states.includes("open"));
    for (let beat = 0; beat < 10; beat += 1) {
      await clock.advance(30_000);
      live.send(": heartbeat\n\n");
    }
    await clock.advance(30_000);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.credentials).toBe("same-origin");
    expect(calls[0]?.headers.Accept).toBe("text/event-stream");
    expect(states).toEqual(["connecting", "open"]);
    stream.close();
  });

  test("silence past the heartbeat timeout reconnects with Last-Event-ID", async () => {
    const clock = new ManualClock();
    const first = fakeStream();
    const second = fakeStream();
    const { fetch, calls } = fakeFetch([async () => eventStream(first.body), async () => eventStream(second.body)]);
    const { states, events, stream } = start(fetch, clock);
    await until("the stream to open", () => states.includes("open"));
    first.send("id: 41\ndata: a\n\n");
    await until("the first event", () => events.length === 1);

    await clock.advance(HEARTBEAT_TIMEOUT_MS - 1);
    expect(calls).toHaveLength(1);
    await clock.advance(1);
    expect(first.cancelled).toBe(true);
    expect(states.at(-1)).toBe("reconnecting");

    // The connection delivered data, so the first wait is the shortest one.
    await clock.advance(1_000);
    await until("the reconnect", () => calls.length === 2);
    expect(calls[1]?.headers["Last-Event-ID"]).toBe("41");
    second.send("id: 42\ndata: b\n\n");
    await until("the event after reconnecting", () => events.length === 2);
    expect(events.map((event) => [event.id, event.data])).toEqual([
      ["41", "a"],
      ["42", "b"],
    ]);
    stream.close();
  });

  test("a stream that ends with status 200 (cut by a proxy) reconnects", async () => {
    const clock = new ManualClock();
    const first = fakeStream();
    const { fetch, calls } = fakeFetch([async () => eventStream(first.body)]);
    const { states, stream } = start(fetch, clock);
    await until("the stream to open", () => states.includes("open"));
    first.send("id: 9\ndata: x\n\n");
    first.end();
    await until("the reconnect wait", () => states.at(-1) === "reconnecting");
    await clock.advance(1_000);
    await until("the reconnect", () => calls.length === 2);
    expect(calls[1]?.headers["Last-Event-ID"]).toBe("9");
    stream.close();
  });

  test("every connection names the user and the tenant the page shows at that moment (Laysi/trustiot-v2#678)", async () => {
    const clock = new ManualClock();
    const { fetch, calls } = fakeFetch([async () => status(503), async () => status(503), async () => status(503)]);
    let shown: ShownSession | undefined = { user: "user-a", tenant: "tenant-a" };
    const stream = openEventStream("/api/x/v1/live", {
      fetch,
      clock,
      random: () => 1,
      onEvent: () => {},
      session: { shown: () => shown, changed: () => {} },
    });
    await until("the first attempt", () => calls.length === 1);
    shown = { user: "user-b", tenant: null };
    await clock.advance(1_000);
    await until("the second attempt", () => calls.length === 2);
    shown = undefined;
    await clock.advance(2_000);
    await until("the third attempt", () => calls.length === 3);
    expect(calls.map((call) => [call.headers[EXPECTED_USER_HEADER], call.headers[EXPECTED_TENANT_HEADER]])).toEqual([
      ["user-a", "tenant-a"],
      ["user-b", EXPECTED_NO_TENANT],
      [undefined, undefined],
    ]);
    stream.close();
    // A stream that is not about a session (`session: null`, as `start` passes) names nothing.
    const other = fakeFetch([async () => status(401)]);
    start(other.fetch, new ManualClock());
    await until("the attempt", () => other.calls.length === 1);
    expect(other.calls[0]?.headers[EXPECTED_USER_HEADER]).toBeUndefined();
    expect(other.calls[0]?.headers[EXPECTED_TENANT_HEADER]).toBeUndefined();
  });

  test("401, 403 and 409 stop instead of retrying", async () => {
    for (const [code, state] of [
      [401, "unauthenticated"],
      [403, "forbidden"],
      [409, "session_changed"],
    ] as const) {
      const clock = new ManualClock();
      const { fetch, calls } = fakeFetch([async () => status(code), async () => status(200)]);
      const { states } = start(fetch, clock);
      await until(`the ${code} to stop the stream`, () => states.includes(state));
      await clock.advance(10 * 60_000);
      expect(calls).toHaveLength(1);
      expect(states).toEqual(["connecting", state]);
    }
  });

  test("401, 403 and 409 release the error body they do not read", async () => {
    for (const code of [401, 403, 409]) {
      let cancelled = false;
      const body = new ReadableStream<Uint8Array>({
        cancel: () => {
          cancelled = true;
        },
      });
      const { fetch } = fakeFetch([
        async () => ({ status: code, headers: new Headers({ "content-type": "application/json" }), body }),
      ]);
      const { states } = start(fetch, new ManualClock());
      await until(`the ${code} to stop the stream`, () => states.length === 2);
      expect(cancelled).toBe(true);
    }
  });

  test("a 409 tells the session, once, that it changed", async () => {
    const clock = new ManualClock();
    const { fetch, calls } = fakeFetch([async () => status(409), async () => status(200)]);
    let changed = 0;
    const states: StreamState[] = [];
    openEventStream("/api/x/v1/live", {
      fetch,
      clock,
      onEvent: () => {},
      onState: (state) => states.push(state),
      session: {
        shown: () => ({ user: "user-a", tenant: "tenant-a" }),
        changed: () => {
          changed += 1;
        },
      },
    });
    await until("the 409 to stop the stream", () => states.includes("session_changed"));
    await clock.advance(10 * 60_000);
    expect(changed).toBe(1);
    expect(calls).toHaveLength(1);
  });

  test("network errors and 5xx retry with a growing wait", async () => {
    const clock = new ManualClock();
    const failure = async (): Promise<StreamResponse> => {
      throw new TypeError("network down");
    };
    const { fetch, calls } = fakeFetch([failure, async () => status(503), failure, failure]);
    start(fetch, clock);
    await until("the first attempt", () => calls.length === 1);
    for (const [index, wait] of [1_000, 2_000, 4_000].entries()) {
      await clock.advance(wait - 1);
      expect(calls).toHaveLength(index + 1);
      await clock.advance(1);
      await until(`attempt ${index + 2}`, () => calls.length === index + 2);
    }
  });

  test("a connection that never answers counts as lost after the timeout", async () => {
    const clock = new ManualClock();
    const { fetch, calls } = fakeFetch([() => new Promise<StreamResponse>(() => {})]);
    const { states } = start(fetch, clock);
    await clock.advance(HEARTBEAT_TIMEOUT_MS);
    await until("the reconnect wait", () => states.at(-1) === "reconnecting");
    await clock.advance(2_000);
    await until("the second attempt", () => calls.length === 2);
  });

  test("a response that is not an event stream is retried", async () => {
    const clock = new ManualClock();
    const { fetch, calls } = fakeFetch([
      async () => ({ status: 200, headers: new Headers({ "content-type": "text/html" }), body: fakeStream().body }),
    ]);
    const { states } = start(fetch, clock);
    await until("the reconnect wait", () => states.at(-1) === "reconnecting");
    expect(states).not.toContain("open");
    await clock.advance(1_000);
    await until("the second attempt", () => calls.length === 2);
  });

  test("close() and an aborted signal stop the stream for good", async () => {
    const clock = new ManualClock();
    const live = fakeStream();
    const { fetch, calls } = fakeFetch([async () => eventStream(live.body)]);
    const { states, stream } = start(fetch, clock);
    await until("the stream to open", () => states.includes("open"));
    stream.close();
    await until("the body to be cancelled", () => live.cancelled);
    await clock.advance(10 * 60_000);
    expect(calls).toHaveLength(1);
    expect(states.at(-1)).toBe("closed");

    const aborted = new AbortController();
    aborted.abort();
    const other = fakeFetch([]);
    const otherStates: StreamState[] = [];
    openEventStream("/x", {
      fetch: other.fetch,
      clock,
      signal: aborted.signal,
      session: null,
      onEvent: () => {},
      onState: (s) => otherStates.push(s),
    });
    await clock.advance(1_000);
    expect(other.calls).toHaveLength(0);
    expect(otherStates).toEqual(["closed"]);
  });

  test("a failing handler does not end the stream", async () => {
    const clock = new ManualClock();
    const live = fakeStream();
    const { fetch } = fakeFetch([async () => eventStream(live.body)]);
    const seen: string[] = [];
    const states: StreamState[] = [];
    const errors = console.error;
    console.error = () => {};
    try {
      const stream = openEventStream("/x", {
        fetch,
        clock,
        session: null,
        onState: (state) => states.push(state),
        onEvent: (event) => {
          seen.push(event.data);
          if (event.data === "bad") throw new Error("handler bug");
        },
      });
      await until("the stream to open", () => states.includes("open"));
      live.send("data: bad\n\ndata: good\n\n");
      await until("both events", () => seen.length === 2);
      expect(states).toEqual(["connecting", "open"]);
      stream.close();
    } finally {
      console.error = errors;
    }
  });
});
