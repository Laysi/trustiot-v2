import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { eventStream, fakeFetch, fakeStream, ManualClock, status, until } from "../test/event-streams.ts";
import {
  type AnswerProgress,
  followAnswer,
  MAX_RESYNCS,
  NO_PROGRESS,
  shownAnswer,
  TEXT_PIECE_UNITS,
} from "./answer-stream.ts";
import type { ConversationRun } from "./conversations.ts";
import { HEARTBEAT_TIMEOUT_MS, MAX_BACKOFF_MS, openEventStream, type StreamResponse, type StreamState } from "./sse.ts";

const OPENAPI = fileURLToPath(new URL("../../../api/openapi.json", import.meta.url));

// Paragraphs as the service releases them while the question runs: each one finished and
// trimmed, the blank line before the next one coming with it.
const FIRST = "Floor 3 used **412 kWh** this week. 😀";
const SECOND = "\n\n| Day | kWh |\n| - | -: |\n| Mon | 60 |";
const THIRD = "\n\nThat is 4% less than last week.";
const ANSWER = FIRST + SECOND + THIRD;

/** A `text` event as the service writes it: `id` is where the piece ends. */
function text(offset: number, piece: string): string {
  return `event: text\nid: ${offset + piece.length}\ndata: ${JSON.stringify({ offset, text: piece })}\n\n`;
}

/** The text events of `answer` from `from` on, in pieces of at most `size` code units. */
function pieces(answer: string, from: number, size: number): string {
  let frames = "";
  for (let offset = from; offset < answer.length; offset += size)
    frames += text(offset, answer.slice(offset, offset + size));
  return frames;
}

const HEARTBEAT = "event: heartbeat\ndata: {}\n\n";

function run(state: ConversationRun["status"]): ConversationRun {
  return {
    id: "3f7c1c1e-6f9e-4c55-8a1d-7b1f8c1e2d3a",
    status: state,
    result: state === "failed" ? "model_unavailable" : null,
    message_key: state === "answered" ? null : `question.${state === "failed" ? "model_unavailable" : state}`,
    started_at: "2026-11-05T08:00:00Z",
    deadline_at: "2026-11-05T08:15:00Z",
    ended_at: "2026-11-05T08:01:00Z",
    stop_requested_at: state === "cancelled" ? "2026-11-05T08:00:30Z" : null,
  };
}

const end = (state: ConversationRun["status"]) => `event: end\ndata: ${JSON.stringify(run(state))}\n\n`;

/** Follows an answer stream whose connections answer with `responses`, in order, on a manual clock. */
function follow(responses: Array<() => Promise<StreamResponse>>) {
  const clock = new ManualClock();
  const { fetch, calls } = fakeFetch(responses);
  const progress: AnswerProgress[] = [];
  const ends: (ConversationRun | null)[] = [];
  const states: StreamState[] = [];
  const follower = followAnswer({
    open: (handlers) =>
      openEventStream("/api/assistant/v1/conversations/c/runs/r/stream", {
        ...handlers,
        fetch,
        clock,
        random: () => 1,
        session: null,
      }),
    onChange: (next) => progress.push(next),
    onEnd: (ended) => ends.push(ended),
    onState: (state) => states.push(state),
  });
  const last = () => progress.at(-1) ?? NO_PROGRESS;
  return { clock, calls, progress, ends, states, follower, last };
}

let warn: ReturnType<typeof spyOn>;
beforeEach(() => {
  warn = spyOn(console, "warn").mockImplementation(() => {});
});
afterEach(() => {
  warn.mockRestore();
});

describe("following an answer stream", () => {
  test("paragraphs show as they arrive, and the answered end shows the whole answer", async () => {
    const live = fakeStream();
    const { calls, last, ends, progress } = follow([async () => eventStream(live.body)]);
    await until("the connection", () => calls.length === 1);
    // The first connection starts at the start of the answer: no Last-Event-ID.
    expect(calls[0]?.headers["Last-Event-ID"]).toBeUndefined();
    live.send(`retry: 1000\n${HEARTBEAT}`);
    live.send(text(0, FIRST));
    await until("the first paragraph", () => last().text === FIRST);
    expect(shownAnswer(last())).toBe(FIRST);
    live.send(text(FIRST.length, SECOND));
    await until("the second paragraph", () => last().text === FIRST + SECOND);
    expect(shownAnswer(last())).toBe(FIRST + SECOND);

    live.send(text(FIRST.length + SECOND.length, THIRD) + end("answered"));
    await until("the end", () => ends.length === 1);
    expect(last()).toMatchObject({ text: ANSWER, ended: true, answered: true });
    expect(shownAnswer(last())).toBe(ANSWER);
    expect(ends[0]?.status).toBe("answered");
    // Every change only added to the end: nothing was shown twice or taken back.
    for (const [index, step] of progress.entries()) {
      expect(step.text.startsWith(progress[index - 1]?.text ?? "")).toBe(true);
    }
  });

  test("silence past the heartbeat timeout reconnects with the position, and an overlapping piece adds nothing twice", async () => {
    const first = fakeStream();
    const second = fakeStream();
    const { clock, calls, last, ends, states } = follow([
      async () => eventStream(first.body),
      async () => eventStream(second.body),
    ]);
    await until("the connection", () => calls.length === 1);
    first.send(HEARTBEAT + text(0, FIRST));
    await until("the first paragraph", () => last().text === FIRST);

    // The proxy stops forwarding and keeps the response open: no data, no error.
    await clock.advance(HEARTBEAT_TIMEOUT_MS - 1);
    expect(calls).toHaveLength(1);
    await clock.advance(1);
    expect(first.cancelled).toBe(true);
    expect(states.at(-1)).toBe("reconnecting");
    await clock.advance(1_000);
    await until("the reconnect", () => calls.length === 2);
    expect(calls[1]?.headers["Last-Event-ID"]).toBe(String(FIRST.length));

    // A piece the page has whole, then one that starts inside what it has.
    const overlap = FIRST.length - 9;
    second.send(text(0, FIRST) + text(overlap, ANSWER.slice(overlap, FIRST.length + 10)));
    second.send(pieces(ANSWER, FIRST.length + 10, 7) + end("answered"));
    await until("the end", () => ends.length === 1);
    expect(last().text).toBe(ANSWER);
    expect(shownAnswer(last())).toBe(ANSWER);
  });

  test("a stream cut part-way with status 200 reconnects and goes on with nothing missing or twice", async () => {
    const first = fakeStream();
    const second = fakeStream();
    const { clock, calls, last, ends } = follow([
      async () => eventStream(first.body),
      async () => eventStream(second.body),
    ]);
    await until("the connection", () => calls.length === 1);
    // Cut in the middle of the second paragraph (the stored answer's rest goes in pieces).
    first.send(text(0, FIRST) + text(FIRST.length, SECOND.slice(0, 11)));
    await until("the pieces", () => last().text === FIRST + SECOND.slice(0, 11));
    first.end();
    await clock.advance(1_000);
    await until("the reconnect", () => calls.length === 2);
    expect(calls[1]?.headers["Last-Event-ID"]).toBe(String(FIRST.length + 11));
    second.send(HEARTBEAT + pieces(ANSWER, FIRST.length + 11, 5) + end("answered"));
    await until("the end", () => ends.length === 1);
    expect(last().text).toBe(ANSWER);
  });

  test("a piece that would leave a hole reopens the stream at once from what the page has", async () => {
    const first = fakeStream();
    const second = fakeStream();
    const { calls, last, ends } = follow([async () => eventStream(first.body), async () => eventStream(second.body)]);
    await until("the connection", () => calls.length === 1);
    first.send(text(0, FIRST) + text(FIRST.length + 4, SECOND.slice(4)));
    await until("the reopening", () => calls.length === 2);
    expect(first.cancelled).toBe(true);
    expect(last().text).toBe(FIRST);
    expect(calls[1]?.headers["Last-Event-ID"]).toBe(String(FIRST.length));
    second.send(pieces(ANSWER, FIRST.length, 4096) + end("answered"));
    await until("the end", () => ends.length === 1);
    expect(last().text).toBe(ANSWER);
  });

  test("an event that does not parse is dropped, warned about without its value, and the stream reopens; it gives up after a few", async () => {
    const streams = Array.from({ length: MAX_RESYNCS + 1 }, () => fakeStream());
    const { clock, calls, last, states } = follow(streams.map((live) => async () => eventStream(live.body)));
    await until("the connection", () => calls.length === 1);
    streams[0]?.send(text(0, FIRST));
    await until("the first paragraph", () => last().text === FIRST);
    for (let attempt = 0; attempt <= MAX_RESYNCS; attempt += 1) {
      await until(`connection ${attempt + 1}`, () => calls.length === attempt + 1);
      streams[attempt]?.send(`event: text\nid: 99\ndata: {"offset": "secret-value", "text": 4}\n\n`);
      await until(`the warning ${attempt + 1}`, () => warn.mock.calls.length === attempt + 1);
    }
    await clock.advance(MAX_BACKOFF_MS * 4);
    expect(calls).toHaveLength(MAX_RESYNCS + 1);
    expect(states.at(-1)).toBe("refused");
    expect(last().text).toBe(FIRST);
    for (const connection of calls.slice(1)) expect(connection.headers["Last-Event-ID"]).toBe(String(FIRST.length));
    expect(JSON.stringify(warn.mock.calls)).not.toContain("secret-value");
    expect(JSON.stringify(warn.mock.calls)).toContain("/offset");
  });

  test("stopped, failed and interrupted questions drop what went out and do not reconnect", async () => {
    for (const outcome of ["cancelled", "failed", "interrupted"] as const) {
      const live = fakeStream();
      const { clock, calls, last, ends } = follow([async () => eventStream(live.body)]);
      await until("the connection", () => calls.length === 1);
      live.send(text(0, FIRST) + text(FIRST.length, SECOND));
      await until("the paragraphs", () => last().text === FIRST + SECOND);
      live.send(end(outcome));
      await until(`the ${outcome} end`, () => ends.length === 1);
      expect(last()).toMatchObject({ text: "", ended: true, answered: false });
      expect(shownAnswer(last())).toBe("");
      expect(ends[0]?.status).toBe(outcome);
      await clock.advance(MAX_BACKOFF_MS * 2);
      expect(calls).toHaveLength(1);
    }
  });

  test("a page opened again while the question runs starts from the start, gets what was written and goes on", async () => {
    const before = fakeStream();
    const earlier = follow([async () => eventStream(before.body)]);
    await until("the first page's connection", () => earlier.calls.length === 1);
    before.send(text(0, FIRST));
    await until("the first page's paragraph", () => earlier.last().text === FIRST);
    // The page is reloaded: its stream closes, and the new page knows nothing of the answer.
    earlier.follower.close();
    expect(before.cancelled).toBe(true);

    const after = fakeStream();
    const reopened = follow([async () => eventStream(after.body)]);
    await until("the new page's connection", () => reopened.calls.length === 1);
    expect(reopened.calls[0]?.headers["Last-Event-ID"]).toBeUndefined();
    // What went out so far, from the start (in pieces), then the rest as it is written.
    after.send(`retry: 1000\n${HEARTBEAT}${pieces(FIRST + SECOND, 0, 10)}`);
    await until("what was written so far", () => reopened.last().text === FIRST + SECOND);
    expect(shownAnswer(reopened.last())).toBe(FIRST + SECOND);
    after.send(text(FIRST.length + SECOND.length, THIRD) + end("answered"));
    await until("the end", () => reopened.ends.length === 1);
    expect(reopened.last().text).toBe(ANSWER);
  });

  test("a resume position the server refuses starts over once from the start of the answer", async () => {
    const first = fakeStream();
    const again = fakeStream();
    const { clock, calls, last, ends, progress } = follow([
      async () => eventStream(first.body),
      async () => status(400),
      async () => eventStream(again.body),
    ]);
    await until("the connection", () => calls.length === 1);
    first.send(text(0, FIRST));
    await until("the first paragraph", () => last().text === FIRST);
    first.end();
    await clock.advance(1_000);
    await until("the start over", () => calls.length === 3);
    expect(calls[1]?.headers["Last-Event-ID"]).toBe(String(FIRST.length));
    expect(calls[2]?.headers["Last-Event-ID"]).toBeUndefined();
    expect(progress.some((step) => step.text === "")).toBe(true);
    again.send(pieces(ANSWER, 0, 4096) + end("answered"));
    await until("the end", () => ends.length === 1);
    expect(last().text).toBe(ANSWER);
  });

  test("a second refusal of the position, and a stream that is gone (404), stop it for good", async () => {
    const first = fakeStream();
    const refusedTwice = follow([
      async () => eventStream(first.body),
      async () => status(400),
      async () => status(400),
    ]);
    await until("the connection", () => refusedTwice.calls.length === 1);
    first.send(text(0, FIRST));
    await until("the paragraph", () => refusedTwice.last().text === FIRST);
    first.end();
    await refusedTwice.clock.advance(1_000);
    await until("the second refusal", () => refusedTwice.states.at(-1) === "refused");
    await refusedTwice.clock.advance(MAX_BACKOFF_MS * 2);
    expect(refusedTwice.calls).toHaveLength(3);

    const gone = follow([async () => status(404)]);
    await until("the refusal", () => gone.states.at(-1) === "refused");
    await gone.clock.advance(MAX_BACKOFF_MS * 2);
    expect(gone.calls).toHaveLength(1);
    expect(gone.ends).toEqual([]);
  });

  test("server errors and refusals of the moment are retried", async () => {
    const live = fakeStream();
    const { clock, calls, last, ends } = follow([
      async () => status(503),
      async () => status(429),
      async () => eventStream(live.body),
    ]);
    await until("the first attempt", () => calls.length === 1);
    await clock.advance(1_000);
    await until("the second attempt", () => calls.length === 2);
    await clock.advance(2_000);
    await until("the third attempt", () => calls.length === 3);
    live.send(pieces(ANSWER, 0, 4096) + end("answered"));
    await until("the end", () => ends.length === 1);
    expect(last().text).toBe(ANSWER);
  });

  test("an end that does not parse ends the stream without an answer", async () => {
    const live = fakeStream();
    const { clock, calls, last, ends } = follow([async () => eventStream(live.body)]);
    await until("the connection", () => calls.length === 1);
    live.send(`${text(0, FIRST)}event: end\ndata: {"status": "maybe"}\n\n`);
    await until("the end", () => ends.length === 1);
    expect(ends).toEqual([null]);
    expect(last()).toMatchObject({ text: "", ended: true, answered: false });
    await clock.advance(MAX_BACKOFF_MS * 2);
    expect(calls).toHaveLength(1);
  });

  test("new text resets the resync budget, so more gaps than MAX_RESYNCS between good pieces still stream the whole answer", async () => {
    const gaps = MAX_RESYNCS + 2;
    const parts = Array.from({ length: gaps + 1 }, (_, index) => `${index === 0 ? "" : "\n\n"}Paragraph ${index}.`);
    const answer = parts.join("");
    const streams = Array.from({ length: gaps + 1 }, () => fakeStream());
    const { calls, last, ends, states } = follow(streams.map((live) => async () => eventStream(live.body)));
    let at = 0;
    for (const [index, part] of parts.entries()) {
      await until(`connection ${index + 1}`, () => calls.length === index + 1);
      expect(calls[index]?.headers["Last-Event-ID"]).toBe(at === 0 ? undefined : String(at));
      // A good piece, then (but for the last connection) one that would leave a hole.
      streams[index]?.send(text(at, part));
      at += part.length;
      if (index < gaps) streams[index]?.send(text(at + 3, "lost"));
    }
    streams[gaps]?.send(end("answered"));
    await until("the end", () => ends.length === 1);
    expect(last().text).toBe(answer);
    expect(states).not.toContain("refused");
  });

  test("streams that close without new text back off, doubling up to the ceiling, and new text starts the waits small again", async () => {
    // Each of these connections sends the server's first frame and closes: no text, no end.
    const empty = async (): Promise<StreamResponse> => {
      const live = fakeStream();
      live.send(`retry: 1000\n${HEARTBEAT}`);
      live.end();
      return eventStream(live.body);
    };
    const withText = async (): Promise<StreamResponse> => {
      const live = fakeStream();
      live.send(`retry: 1000\n${HEARTBEAT}${text(0, FIRST)}`);
      live.end();
      return eventStream(live.body);
    };
    const waits = [1_000, 2_000, 4_000, 8_000, 16_000, MAX_BACKOFF_MS, MAX_BACKOFF_MS];
    const { clock, calls, states, last } = follow([...waits.map(() => empty), withText, empty, empty]);
    const reconnecting = () => states.filter((state) => state === "reconnecting").length;
    for (const [index, wait] of waits.entries()) {
      await until(`connection ${index + 1} to close`, () => reconnecting() === index + 1);
      await clock.advance(wait - 1);
      expect(calls).toHaveLength(index + 1);
      await clock.advance(1);
      await until(`connection ${index + 2}`, () => calls.length === index + 2);
    }
    // That connection delivered text: the next wait is the shortest again, then grows.
    await until("the text", () => last().text === FIRST);
    for (const [step, wait] of [1_000, 2_000].entries()) {
      const before = calls.length;
      await until(`close ${before}`, () => reconnecting() === before);
      await clock.advance(wait - 1);
      expect(calls).toHaveLength(before);
      await clock.advance(1);
      await until(`connection ${before + 1} (step ${step})`, () => calls.length === before + 1);
    }
  });

  test("close() stops for good: nothing more is reported and nothing reconnects", async () => {
    const live = fakeStream();
    const { clock, calls, progress, follower } = follow([async () => eventStream(live.body)]);
    await until("the connection", () => calls.length === 1);
    live.send(text(0, FIRST));
    await until("the paragraph", () => progress.length === 1);
    follower.close();
    expect(live.cancelled).toBe(true);
    await clock.advance(HEARTBEAT_TIMEOUT_MS + MAX_BACKOFF_MS);
    expect(progress).toHaveLength(1);
    expect(calls).toHaveLength(1);
  });
});

describe("the API contract on the size of a piece", () => {
  test("TEXT_PIECE_UNITS is the size api/openapi.json says the service cuts pieces at, and the only place it cuts", () => {
    const spec = JSON.parse(readFileSync(OPENAPI, "utf8"));
    const schema = spec.components.schemas.AnswerStreamText;
    const flat = (description: string) => description.replace(/\s+/g, " ");
    const numbers = (description: string, pattern: RegExp) => {
      const match = flat(description).match(pattern);
      if (match === null) throw new Error(`no ${pattern} in "${description}"`);
      return match.slice(1).map((digits) => Number(digits.replaceAll(",", "")));
    };
    const number = (description: string, pattern: RegExp) => numbers(description, pattern)[0];
    const text: string = schema.properties.text.description;
    expect(number(text, /at most ([\d,]+) UTF-16 code units/)).toBe(TEXT_PIECE_UNITS);
    // `whole` rests on this: a shorter piece ends at the end of what the server had.
    expect(numbers(text, /only at ([\d,]+) units, or at ([\d,]+) when the next character takes two/)).toEqual([
      TEXT_PIECE_UNITS,
      TEXT_PIECE_UNITS - 1,
    ]);
    expect(number(text, /a piece shorter than ([\d,]+) units ends at the end of the text the server had/)).toBe(
      TEXT_PIECE_UNITS - 1,
    );
    expect(number(schema.description, /a piece shorter than ([\d,]+) UTF-16 code units ends at the end/)).toBe(
      TEXT_PIECE_UNITS - 1,
    );
  });
});

describe("shownAnswer", () => {
  test("what the server had is shown whole; after a piece cut to size, up to the paragraph before it; without an answer, nothing", () => {
    expect(shownAnswer({ text: FIRST, ended: false, answered: false, whole: true })).toBe(FIRST);
    expect(shownAnswer({ text: FIRST + SECOND, ended: false, answered: false, whole: true })).toBe(FIRST + SECOND);
    expect(shownAnswer({ text: "Half a para", ended: false, answered: false, whole: false })).toBe("");
    expect(shownAnswer({ text: `${FIRST}\n\n| Day | k`, ended: false, answered: false, whole: false })).toBe(FIRST);
    expect(shownAnswer({ text: ANSWER, ended: true, answered: true, whole: false })).toBe(ANSWER);
    expect(shownAnswer({ text: ANSWER, ended: true, answered: false, whole: true })).toBe("");
  });

  test("a long text the server cut into pieces shows paragraph by paragraph, never half of one", async () => {
    const long = `${FIRST}\n\n**${"x".repeat(5000)}** done.`;
    const live = fakeStream();
    const { calls, last, progress } = follow([async () => eventStream(live.body)]);
    await until("the connection", () => calls.length === 1);
    // The service's cut: 4,096 code units, then the rest.
    live.send(text(0, long.slice(0, TEXT_PIECE_UNITS)));
    await until("the first piece", () => progress.length === 1);
    expect(last().whole).toBe(false);
    expect(shownAnswer(last())).toBe(FIRST);
    live.send(text(TEXT_PIECE_UNITS, long.slice(TEXT_PIECE_UNITS)));
    await until("the rest", () => last().text === long);
    expect(last().whole).toBe(true);
    expect(shownAnswer(last())).toBe(long);
  });
});
