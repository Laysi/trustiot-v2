import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { HEALTHY_CONNECTION_MS, HEARTBEAT_TIMEOUT_MS } from "../../api/sse.ts";

import { conversationPath } from "../../app/paths.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { renderAt } from "../../test/render.tsx";

// The conversation page with the answer stream (Laysi/trustiot-v2#1046): the answer shows as it
// is written, the stream reconnects where it was cut, and every way a question ends is the
// same as before streaming (ConversationTurn.test.tsx). The heartbeat timeout itself is covered
// with a manual clock in api/answer-stream.test.ts; here a cut stream reconnects after the
// stream's first backoff (about a second of real time), so waits are bounded generously: the
// condition is what is awaited, not the time.

afterEach(() => {
  cleanup();
});

const WAIT = { timeout: 60_000 };
const SLOW_TEST_MS = 120_000;

// Paragraphs as the service releases them while the question runs: each one finished and
// trimmed, the blank line before the next one coming with it, so the newest paragraph shows
// before the next one is written.
const FIRST = "Floor 3 used **412 kWh** this week.";
const SECOND = "\n\n| Day | kWh |\n| - | -: |\n| Mon | 60 |";
const THIRD = "\n\nThat is 4% less than last week.";

function setUp() {
  const server = new FakeQuestionServer();
  const conversation = server.add({
    title: "Energy this week",
    messages: [
      { kind: "question", text: "How did floor 3 use energy this week?" },
      { kind: "answer", text: "Floor 3 used 412 kWh." },
    ],
  });
  return { server, conversation };
}

function open(server: FakeQuestionServer, id: string) {
  return renderAt(conversationPath(id), { conversations: server.api() });
}

async function ask(text: string) {
  // The first test of a run loads the conversation page lazily, which took more than Testing
  // Library's default second on a workstation (Laysi/trustiot-v2#1138).
  const input = await screen.findByRole("textbox", { name: "Keep asking TrustIoT" }, WAIT);
  act(() => {
    fireEvent.change(input, { target: { value: text } });
  });
  act(() => {
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
  });
}

const box = () => screen.getByRole("textbox", { name: "Keep asking TrustIoT" }) as HTMLTextAreaElement;
const pending = () => screen.queryByRole("list", { name: "Question being answered" });
const writing = () => screen.queryByRole("article", { name: "TrustIoT's answer, still being written" });
const answers = () => screen.getAllByRole("article", { name: "TrustIoT's answer" });

async function streamOpen(server: FakeQuestionServer) {
  await waitFor(() => expect(server.openStreams).toBe(1), WAIT);
}

describe("an answer being written", () => {
  test(
    "shows paragraph by paragraph before the question ends, then once as the answer in the thread",
    async () => {
      const { server, conversation } = setUp();
      open(server, conversation.id);
      await ask("And last week?");
      await streamOpen(server);
      expect(within(pending() as HTMLElement).getByText("Looking up readings…")).toBeTruthy();

      act(() => server.write(conversation.id, FIRST));
      const article = await screen.findByRole("article", { name: "TrustIoT's answer, still being written" }, WAIT);
      expect(article.getAttribute("aria-busy")).toBe("true");
      // Drawn as an answer is: Markdown through the answer's rules.
      expect(within(article).getByText("412 kWh").tagName).toBe("STRONG");
      expect(within(article).getByText("Writing the answer…")).toBeTruthy();
      expect(within(pending() as HTMLElement).queryByText("Looking up readings…")).toBeNull();
      expect(within(pending() as HTMLElement).getByText("And last week?")).toBeTruthy();

      act(() => server.write(conversation.id, SECOND));
      await waitFor(() => expect(within(writing() as HTMLElement).getByRole("table")).toBeTruthy(), WAIT);
      expect(server.latestRun(conversation.id)?.status).toBe("running");

      act(() => server.answer(conversation.id, FIRST + SECOND + THIRD));
      await waitFor(() => expect(pending()).toBeNull(), WAIT);
      const last = answers().at(-1) as HTMLElement;
      expect(last.textContent).toContain("That is 4% less than last week.");
      expect(answers()).toHaveLength(2);
      expect(screen.getAllByText("That is 4% less than last week.")).toHaveLength(1);
      expect(writing()).toBeNull();
      // One stream, opened from the start of the answer.
      expect(server.streamOpens).toEqual([{ runId: server.latestRun(conversation.id)?.id ?? "", lastEventId: null }]);
    },
    SLOW_TEST_MS,
  );

  test(
    "without paragraphs while it runs (the answer tool off), the waiting line stays until the answer",
    async () => {
      const { server, conversation } = setUp();
      open(server, conversation.id);
      await ask("A text answer");
      await streamOpen(server);
      act(() => server.heartbeat());
      expect(within(pending() as HTMLElement).getByText("Looking up readings…")).toBeTruthy();
      expect(writing()).toBeNull();
      act(() => server.answer(conversation.id, "All at once."));
      await waitFor(() => expect(pending()).toBeNull(), WAIT);
      expect(answers().at(-1)?.textContent).toContain("All at once.");
    },
    SLOW_TEST_MS,
  );

  test(
    "an answer's images and outside links stay text while it is written",
    async () => {
      const { server, conversation } = setUp();
      open(server, conversation.id);
      await ask("Show me");
      await streamOpen(server);
      act(() =>
        server.write(
          conversation.id,
          "See ![chart](https://example.invalid/leak.png?d=1) and [the docs](https://example.invalid/x).",
        ),
      );
      const article = await screen.findByRole("article", { name: "TrustIoT's answer, still being written" }, WAIT);
      expect(article.querySelector("img")).toBeNull();
      expect(article.querySelector("a")).toBeNull();
      expect(article.textContent).toContain("the docs");
    },
    SLOW_TEST_MS,
  );

  test(
    "a stream cut with status 200 reconnects from where it was, with nothing missing or twice",
    async () => {
      const { server, conversation } = setUp();
      open(server, conversation.id);
      await ask("And last week?");
      await streamOpen(server);
      act(() => server.write(conversation.id, FIRST));
      await screen.findByRole("article", { name: "TrustIoT's answer, still being written" }, WAIT);

      act(() => server.cutStreams());
      // Written while the page was not connected.
      act(() => server.write(conversation.id, SECOND));
      await waitFor(() => expect(server.streamOpens).toHaveLength(2), WAIT);
      expect(server.streamOpens[1]?.lastEventId).toBe(String(FIRST.length));
      await waitFor(() => expect(within(writing() as HTMLElement).getByRole("table")).toBeTruthy(), WAIT);
      expect(within(writing() as HTMLElement).getAllByText("412 kWh")).toHaveLength(1);

      act(() => server.answer(conversation.id, FIRST + SECOND + THIRD));
      await waitFor(() => expect(pending()).toBeNull(), WAIT);
      expect(screen.getAllByText("412 kWh")).toHaveLength(1);
    },
    SLOW_TEST_MS,
  );

  test(
    "a page opened while the question runs shows what was written so far and goes on",
    async () => {
      const { server, conversation } = setUp();
      server.runs.set("running-run", {
        id: "running-run",
        conversationId: conversation.id,
        text: "Asked before the page opened",
        status: "running",
        startedAt: "2026-11-05T01:00:00Z",
      });
      server.write(conversation.id, FIRST);
      open(server, conversation.id);
      const article = await screen.findByRole("article", { name: "TrustIoT's answer, still being written" }, WAIT);
      expect(within(article).getByText("412 kWh")).toBeTruthy();
      // Its text is not known here (it is stored with the answer): only the answer being written.
      expect(within(pending() as HTMLElement).queryByText("Asked before the page opened")).toBeNull();
      expect(server.streamOpens[0]?.lastEventId).toBeNull();

      act(() => server.write(conversation.id, SECOND));
      await waitFor(() => expect(within(writing() as HTMLElement).getByRole("table")).toBeTruthy(), WAIT);
      act(() => server.answer(conversation.id, FIRST + SECOND + THIRD));
      await waitFor(() => expect(pending()).toBeNull(), WAIT);
      expect(within(answers().at(-1) as HTMLElement).getByText("412 kWh")).toBeTruthy();
    },
    SLOW_TEST_MS,
  );
});

describe("a question that ends without an answer while it is written", () => {
  test(
    "stopped: what was written goes, the page says it stopped and the question is back in the box",
    async () => {
      const { server, conversation } = setUp();
      open(server, conversation.id);
      await ask("A long question");
      await streamOpen(server);
      act(() => server.write(conversation.id, FIRST));
      await screen.findByRole("article", { name: "TrustIoT's answer, still being written" }, WAIT);
      act(() => {
        fireEvent.click(screen.getByRole("button", { name: "Stop" }));
      });
      await screen.findByRole("button", { name: "Stopping…" }, WAIT);
      // Still shown while it stops.
      expect(writing()).toBeTruthy();
      act(() => server.settleStop(conversation.id));
      await screen.findByText(/^Stopped\. Nothing of this question was kept/, {}, WAIT);
      expect(pending()).toBeNull();
      expect(screen.queryByText("412 kWh")).toBeNull();
      expect(box().value).toBe("A long question");
      expect(answers()).toHaveLength(1);
    },
    SLOW_TEST_MS,
  );

  test(
    "failed or interrupted: what was written goes and the service's message shows",
    async () => {
      for (const [end, message] of [
        ["fail", "The assistant is unavailable right now. Please try again later."],
        ["interrupt", "interrupted"],
      ] as const) {
        const { server, conversation } = setUp();
        open(server, conversation.id);
        await ask(`Question that will ${end}`);
        await streamOpen(server);
        act(() => server.write(conversation.id, FIRST));
        await screen.findByRole("article", { name: "TrustIoT's answer, still being written" }, WAIT);
        act(() => (end === "fail" ? server.fail(conversation.id) : server.interrupt(conversation.id)));
        const alert = await screen.findByRole("alert", {}, WAIT);
        expect(alert.textContent?.toLowerCase()).toContain(message.toLowerCase());
        expect(pending()).toBeNull();
        expect(screen.queryByText("412 kWh")).toBeNull();
        expect(box().value).toBe(`Question that will ${end}`);
        expect(answers()).toHaveLength(1);
        cleanup();
      }
    },
    SLOW_TEST_MS,
  );

  test(
    "leaving the page while a stream is open aborts its request and leaves no stream timer running",
    async () => {
      // The stream's own timers (the heartbeat watchdog and the healthy-connection mark), seen
      // through plain wrappers: a mock function would make Testing Library expect fake timers.
      const realSetTimeout = globalThis.setTimeout;
      const realClearTimeout = globalThis.clearTimeout;
      const streamTimers = new Set<unknown>();
      const cleared = new Set<unknown>();
      globalThis.setTimeout = ((callback: () => void, ms?: number, ...rest: unknown[]) => {
        const handle = realSetTimeout(callback, ms, ...rest);
        if (ms === HEARTBEAT_TIMEOUT_MS || ms === HEALTHY_CONNECTION_MS) streamTimers.add(handle);
        return handle;
      }) as typeof globalThis.setTimeout;
      globalThis.clearTimeout = ((handle?: Parameters<typeof clearTimeout>[0]) => {
        cleared.add(handle);
        realClearTimeout(handle);
      }) as typeof globalThis.clearTimeout;
      try {
        const { server, conversation } = setUp();
        open(server, conversation.id);
        await ask("A question left behind");
        await streamOpen(server);
        act(() => server.write(conversation.id, FIRST));
        await screen.findByRole("article", { name: "TrustIoT's answer, still being written" }, WAIT);
        // The proxy goes quiet: the stream stays open, with nothing more to read and no end.
        server.stallStreams();
        const signal = server.streamSignals.at(-1) as AbortSignal;
        expect(signal.aborted).toBe(false);
        expect(streamTimers.size).toBeGreaterThan(0);
        const opens = server.streamOpens.length;

        cleanup();
        await waitFor(() => expect(server.openStreams).toBe(0), WAIT);
        expect(signal.aborted).toBe(true);
        await waitFor(() => expect([...streamTimers].filter((timer) => !cleared.has(timer))).toEqual([]), WAIT);
        // Leaving the page does not stop the question, and nothing opened the stream again.
        expect(server.latestRun(conversation.id)?.status).toBe("running");
        expect(server.streamOpens).toHaveLength(opens);
      } finally {
        globalThis.setTimeout = realSetTimeout;
        globalThis.clearTimeout = realClearTimeout;
      }
    },
    SLOW_TEST_MS,
  );

  test(
    "a newer question from another tab replaces the one being written, and its stream closes",
    async () => {
      const { server, conversation } = setUp();
      open(server, conversation.id);
      await ask("The first question");
      await streamOpen(server);
      act(() => server.write(conversation.id, FIRST));
      await screen.findByRole("article", { name: "TrustIoT's answer, still being written" }, WAIT);
      // The first stream stays open with nothing more to read and no `end`, so only the page can
      // close it.
      server.stallStreams();
      const first = server.streamSignals.at(-1) as AbortSignal;
      // Another tab: the first question is answered, and a newer one starts.
      act(() => {
        server.answer(conversation.id, `${FIRST}\n\nDone.`);
        server.runs.set("newer-run", {
          id: "newer-run",
          conversationId: conversation.id,
          text: "From another tab",
          status: "running",
          startedAt: "2099-01-01T00:00:00Z",
        });
      });
      await waitFor(() => expect(server.streamOpens.at(-1)?.runId).toBe("newer-run"), WAIT);
      await waitFor(() => expect(first.aborted).toBe(true), WAIT);
      await waitFor(() => expect(writing()).toBeNull(), WAIT);
      expect(within(pending() as HTMLElement).getByText("Looking up readings…")).toBeTruthy();
      expect(server.openStreams).toBe(1);
    },
    SLOW_TEST_MS,
  );
});
