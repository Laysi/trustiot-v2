import { describe, expect, test } from "bun:test";

import { eventStream, fakeFetch, fakeStream, ManualClock, status, until } from "../test/event-streams.ts";
import { EXPECTED_NO_TENANT, EXPECTED_TENANT_HEADER, EXPECTED_USER_HEADER, type ShownSession } from "./client.ts";
import {
  backoff,
  EventStreamParser,
  HEALTHY_CONNECTION_MS,
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

  test("an id takes effect when its event is dispatched, not when its line is read", () => {
    const parser = new EventStreamParser("4096");
    // The connection is cut after the id line, before the data line ends.
    expect(parser.push('event: text\nid: 8192\ndata: {"offset":4096,')).toEqual([]);
    expect(parser.lastEventId).toBe("4096");
    expect(parser.push('"text":"..."}\n\n')).toEqual([
      { type: "text", data: '{"offset":4096,"text":"..."}', id: "8192" },
    ]);
    expect(parser.lastEventId).toBe("8192");
    // An id in a frame without data is no event: it does not move the position by itself...
    expect(parser.push("id: 9000\n\n")).toEqual([]);
    expect(parser.lastEventId).toBe("8192");
    // ...and the next event, without an id of its own, carries it.
    expect(parser.push("data: next\n\n")).toEqual([{ type: "message", data: "next", id: "9000" }]);
    expect(parser.lastEventId).toBe("9000");
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

describe("openEventStream: a starting position and statuses that stop it", () => {
  test("lastEventId is sent on the first connection, and an event's id replaces it", async () => {
    const clock = new ManualClock();
    const first = fakeStream();
    const { fetch, calls } = fakeFetch([async () => eventStream(first.body)]);
    const events: ServerSentEvent[] = [];
    const states: StreamState[] = [];
    const stream = openEventStream("/api/x/v1/live", {
      fetch,
      clock,
      random: () => 1,
      lastEventId: "120",
      onEvent: (event) => events.push(event),
      onState: (state) => states.push(state),
      session: null,
    });
    await until("the stream to open", () => states.includes("open"));
    expect(calls[0]?.headers["Last-Event-ID"]).toBe("120");
    first.send("id: 130\ndata: x\n\n");
    await until("the event", () => events.length === 1);
    expect(events[0]?.id).toBe("130");
    first.end();
    await until("the reconnect wait", () => states.at(-1) === "reconnecting");
    await clock.advance(1_000);
    await until("the reconnect", () => calls.length === 2);
    expect(calls[1]?.headers["Last-Event-ID"]).toBe("130");
    stream.close();
  });

  test("a connection cut inside an event resumes from the last event that was dispatched", async () => {
    const clock = new ManualClock();
    const first = fakeStream();
    const { fetch, calls } = fakeFetch([async () => eventStream(first.body)]);
    const { states, events, stream } = start(fetch, clock);
    await until("the stream to open", () => states.includes("open"));
    first.send("id: 10\ndata: a\n\nevent: text\nid: 20\ndata: half");
    await until("the first event", () => events.length === 1);
    first.end();
    await until("the reconnect wait", () => states.at(-1) === "reconnecting");
    await clock.advance(1_000);
    await until("the reconnect", () => calls.length === 2);
    expect(calls[1]?.headers["Last-Event-ID"]).toBe("10");
    stream.close();
  });

  test("connections that end without moving the stream forward wait longer each time; one open long enough does not", async () => {
    const clock = new ManualClock();
    // Each answers with a heartbeat only (no progress by default) and closes at once.
    const heartbeatOnly = async (): Promise<StreamResponse> => {
      const live = fakeStream();
      live.send("event: heartbeat\ndata: {}\n\n: comment\n\n");
      live.end();
      return eventStream(live.body);
    };
    const quiet = fakeStream();
    const { fetch, calls } = fakeFetch([
      heartbeatOnly,
      heartbeatOnly,
      heartbeatOnly,
      async () => eventStream(quiet.body),
    ]);
    const { states, stream } = start(fetch, clock);
    const reconnecting = () => states.filter((state) => state === "reconnecting").length;
    for (const [index, wait] of [1_000, 2_000, 4_000].entries()) {
      await until(`connection ${index + 1} to close`, () => reconnecting() === index + 1);
      await clock.advance(wait - 1);
      expect(calls).toHaveLength(index + 1);
      await clock.advance(1);
      await until(`connection ${index + 2}`, () => calls.length === index + 2);
    }
    // This one stays open, quiet but for heartbeats, as long as the longest wait: it was healthy.
    await until("the fourth connection to open", () => states.filter((state) => state === "open").length === 4);
    for (let beat = 0; beat < 2; beat += 1) {
      await clock.advance(HEALTHY_CONNECTION_MS / 2);
      quiet.send("event: heartbeat\ndata: {}\n\n");
    }
    quiet.end();
    await until("connection 4 to close", () => reconnecting() === 4);
    await clock.advance(999);
    expect(calls).toHaveLength(4);
    await clock.advance(1);
    await until("connection 5", () => calls.length === 5);
    stream.close();
  });

  test("progressed decides what moves the stream forward", async () => {
    const clock = new ManualClock();
    const ignored = async (): Promise<StreamResponse> => {
      const live = fakeStream();
      live.send("id: 1\ndata: same\n\n");
      live.end();
      return eventStream(live.body);
    };
    const { fetch, calls } = fakeFetch([ignored, ignored, ignored]);
    const states: StreamState[] = [];
    const stream = openEventStream("/api/x/v1/live", {
      fetch,
      clock,
      random: () => 1,
      onEvent: () => {},
      progressed: () => false,
      onState: (state) => states.push(state),
      session: null,
    });
    const reconnecting = () => states.filter((state) => state === "reconnecting").length;
    for (const [index, wait] of [1_000, 2_000].entries()) {
      await until(`connection ${index + 1} to close`, () => reconnecting() === index + 1);
      await clock.advance(wait - 1);
      expect(calls).toHaveLength(index + 1);
      await clock.advance(1);
      await until(`connection ${index + 2}`, () => calls.length === index + 2);
    }
    stream.close();
  });

  test("a status in stopOn stops the stream as refused, releases the body and says which status", async () => {
    for (const code of [400, 404]) {
      const clock = new ManualClock();
      const live = fakeStream();
      const { fetch, calls } = fakeFetch([
        async () => ({ status: code, headers: new Headers({ "content-type": "application/json" }), body: live.body }),
      ]);
      const states: StreamState[] = [];
      const refused: number[] = [];
      openEventStream("/api/x/v1/live", {
        fetch,
        clock,
        random: () => 1,
        stopOn: [400, 404],
        onRefused: (refusal) => refused.push(refusal),
        onEvent: () => {},
        onState: (state) => states.push(state),
        session: null,
      });
      await until(`the refusal of ${code}`, () => refused.length === 1);
      await clock.advance(MAX_BACKOFF_MS * 2);
      expect(refused).toEqual([code]);
      expect(states).toEqual(["connecting", "refused"]);
      expect(calls).toHaveLength(1);
      expect(live.cancelled).toBe(true);
    }
  });

  test("a status not in stopOn is retried as before", async () => {
    const clock = new ManualClock();
    const { fetch, calls } = fakeFetch([async () => status(404), async () => status(404)]);
    const states: StreamState[] = [];
    const stream = openEventStream("/api/x/v1/live", {
      fetch,
      clock,
      random: () => 1,
      stopOn: [400],
      onEvent: () => {},
      onState: (state) => states.push(state),
      session: null,
    });
    await until("the first attempt", () => states.at(-1) === "reconnecting");
    await clock.advance(1_000);
    await until("the retry", () => calls.length === 2);
    stream.close();
  });

  test("once a handler closes the stream, the rest of the same chunk does not reach it", async () => {
    const clock = new ManualClock();
    const live = fakeStream();
    const { fetch } = fakeFetch([async () => eventStream(live.body)]);
    const seen: string[] = [];
    const states: StreamState[] = [];
    const stream = openEventStream("/api/x/v1/live", {
      fetch,
      clock,
      onEvent: (event) => {
        seen.push(event.data);
        if (event.data === "last") stream.close();
      },
      onState: (state) => states.push(state),
      session: null,
    });
    await until("the stream to open", () => states.includes("open"));
    live.send("data: first\n\ndata: last\n\ndata: after\n\n");
    await until("the stream to close", () => states.includes("closed"));
    await clock.advance(MAX_BACKOFF_MS * 2);
    expect(seen).toEqual(["first", "last"]);
  });
});
