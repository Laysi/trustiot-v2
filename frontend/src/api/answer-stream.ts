import type { Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import { type ConversationRun, ConversationRunSchema } from "./conversations.ts";
import type { components } from "./schema.gen.ts";
import type { EventStream, ServerSentEvent, StreamOptions, StreamState } from "./sse.ts";

// The answer stream of a question (Laysi/trustiot-v2#147, services/assistant/README.md
// 「回答的串流」), as the conversation page follows it (Laysi/trustiot-v2#1046):
//
// - `text` events carry the answer in pieces: `{offset, text}`, where `offset` is where the piece
//   starts in the answer, in UTF-16 code units (a browser string's length), and the event's `id`
//   is where it ends. The page keeps the answer it has and appends only what goes past its end,
//   so a piece that overlaps what it already has (after a reconnect) adds nothing twice.
// - `end` carries the run: `answered` means the text is the whole answer; any other outcome
//   means the text that went out is no answer, and the page drops it.
// - A stream that closes without `end`, or goes silent past the heartbeat timeout (a proxy can cut
//   it and keep the status 200), is reopened by `openEventStream` with the last `id` as
//   `Last-Event-ID`, so the server goes on from there. Only new text counts as progress for its
//   backoff: a stream that keeps closing without any waits longer each time.
// - A piece that starts past the end of what the page has (it would leave a hole), or an event
//   that does not parse, reopens the stream from the page's own position, at most
//   `MAX_RESYNCS` times in a row (new text resets the count); a resume position the server
//   refuses (400) starts over once from the start of the answer; a stream that is gone (404)
//   stops. In each of those cases the page still
//   reads the conversation's `latest_run`, which is what ends the question there.

const TextSchema = z.object({ offset: z.number().int().nonnegative(), text: z.string() });

export type AnswerStreamTextMatchesSpec = Assert<
  Equal<z.output<typeof TextSchema>, components["schemas"]["AnswerStreamText"]>
>;

/** What the page has of an answer being streamed. */
export interface AnswerProgress {
  /** The answer from its start, as far as it arrived. */
  text: string;
  /** Whether the stream said the question ended (`end`). */
  ended: boolean;
  /** Whether it ended answered: `text` is then the whole answer. */
  answered: boolean;
  /**
   * Whether `text` ends where what the server had ended (its last piece was shorter than the
   * server cuts pieces): a finished paragraph while the question runs, or the end of the answer.
   * A piece the server cut to size can end in the middle of a paragraph.
   */
  whole: boolean;
}

export const NO_PROGRESS: AnswerProgress = { text: "", ended: false, answered: false, whole: true };

/**
 * The most UTF-16 code units the service puts in one `text` event (`TEXT_CHUNK_UNITS`); it cuts
 * a longer text into pieces of this size, one less when a character of two units would cross it,
 * and nowhere else. That is the API's contract (`AnswerStreamText` in api/openapi.json), which
 * answer-stream.test.ts checks this number against.
 */
export const TEXT_PIECE_UNITS = 4096;

/**
 * How many times in a row one follower reopens the stream from its own position before it gives
 * up; new text resets the count, so a long answer over a flaky network keeps streaming.
 */
export const MAX_RESYNCS = 3;

/** The statuses that end the answer stream instead of a retry: a refused resume position, a stream that is gone. */
export const ANSWER_STREAM_STOP = [400, 404] as const;

/** What `followAnswer` passes when it opens the stream; the caller adds the page's session. */
export type AnswerStreamHandlers = Required<
  Pick<StreamOptions, "onEvent" | "onState" | "onRefused" | "lastEventId" | "stopOn" | "progressed">
>;

export interface FollowAnswerOptions {
  /** Opens the stream with these handlers (`ConversationApi.streamAnswer`, with the page's session). */
  open(handlers: AnswerStreamHandlers): EventStream;
  /** Every change of what the page has. */
  onChange(progress: AnswerProgress): void;
  /** Once, when the stream said the question ended; the run is `null` when its `end` did not parse. */
  onEnd?(run: ConversationRun | null): void;
  /** The state of the stream that is open now. */
  onState?(state: StreamState): void;
}

export interface AnswerFollower {
  close(): void;
}

/** An event's JSON data through `schema`; `undefined`, with a warning naming the event type and the issue paths (never a value), when it does not parse. */
function parseData<T extends z.ZodType>(schema: T, event: ServerSentEvent): z.output<T> | undefined {
  let data: unknown;
  try {
    data = JSON.parse(event.data);
  } catch {
    console.warn(`answer stream: a ${event.type} event that is not JSON`);
    return undefined;
  }
  const parsed = schema.safeParse(data);
  if (!parsed.success) {
    const paths = parsed.error.issues.map((issue) => `/${issue.path.join("/")}`);
    console.warn(`answer stream: a ${event.type} event that does not parse`, paths);
    return undefined;
  }
  return parsed.data;
}

/** Follows the answer stream of one question until it ends, is closed, or cannot go on. */
export function followAnswer(options: FollowAnswerOptions): AnswerFollower {
  let text = "";
  let finished = false;
  let stream: EventStream | null = null;
  // Each connection's handlers check that they are still the current one: events of a stream
  // that was dropped (closed for a resync) never reach the answer.
  let generation = 0;
  let resyncs = 0;
  let restarted = false;
  let whole = true;
  // Whether the event just received added text: what `progressed` tells the stream's backoff.
  let advanced = false;

  const report = (ended = false, answered = false) => options.onChange({ text, ended, answered, whole });

  function drop() {
    generation += 1;
    const current = stream;
    stream = null;
    current?.close();
  }

  function connect() {
    generation += 1;
    const current = generation;
    const live = () => !finished && current === generation;
    stream = options.open({
      lastEventId: text.length > 0 ? String(text.length) : "",
      stopOn: ANSWER_STREAM_STOP,
      onEvent: (event) => {
        if (live()) receive(event);
      },
      progressed: () => {
        const moved = advanced && live();
        advanced = false;
        return moved;
      },
      onState: (state) => {
        if (live()) options.onState?.(state);
      },
      onRefused: (status) => {
        if (live()) refused(status);
      },
    });
  }

  /** Opens the stream again from the end of what the page has. */
  function resync() {
    drop();
    if (resyncs >= MAX_RESYNCS) {
      finished = true;
      options.onState?.("refused");
      return;
    }
    resyncs += 1;
    connect();
  }

  function refused(status: number) {
    stream = null;
    if (status === 400 && text.length > 0 && !restarted) {
      // The server does not take the page's position: start over from the start of the answer.
      restarted = true;
      text = "";
      whole = true;
      report();
      connect();
      return;
    }
    finished = true;
    options.onState?.("refused");
  }

  function finish(run: ConversationRun | null) {
    drop();
    finished = true;
    const answered = run?.status === "answered";
    // A question that ended without an answer kept nothing: what went out of it is no answer.
    if (!answered) text = "";
    report(true, answered);
    options.onEnd?.(run);
  }

  function receive(event: ServerSentEvent) {
    switch (event.type) {
      case "text": {
        const piece = parseData(TextSchema, event);
        if (piece === undefined || piece.offset > text.length) {
          // A piece the page cannot read, or one that would leave a hole: go on from what the page has.
          resync();
          return;
        }
        const end = piece.offset + piece.text.length;
        if (end <= text.length) return;
        text += piece.text.slice(text.length - piece.offset);
        // Only a piece the server cut to size may stop inside a paragraph.
        whole = piece.text.length < TEXT_PIECE_UNITS - 1;
        // The stream is going forward: the resync budget counts failures in a row.
        resyncs = 0;
        advanced = true;
        report();
        return;
      }
      case "end":
        finish(parseData(ConversationRunSchema, event) ?? null);
        return;
      default:
        // `heartbeat`, and any event type a later server adds.
        return;
    }
  }

  connect();
  return {
    close() {
      finished = true;
      drop();
    },
  };
}

/**
 * The part of an answer the page shows: all of it once the question ended answered, or while it
 * runs when the text ends where what the server had ends (`whole`: the service releases whole
 * paragraphs, trimmed, while the question runs). After a piece the server cut to size (a long
 * resume, the rest of the stored answer), only up to the last paragraph break: a paragraph cut in
 * the middle could show as broken Markdown (an unfinished `**`, half a table) until its rest
 * arrives.
 */
export function shownAnswer(progress: AnswerProgress): string {
  if (progress.ended) return progress.answered ? progress.text : "";
  if (progress.whole) return progress.text;
  const paragraph = progress.text.lastIndexOf("\n\n");
  return paragraph < 0 ? "" : progress.text.slice(0, paragraph);
}
