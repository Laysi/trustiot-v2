import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath } from "../../app/paths.ts";
import { fakeError } from "../../test/fake-dashboards.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { renderAt } from "../../test/render.tsx";
import { until } from "../../test/until.ts";

afterEach(() => {
  cleanup();
});

// The page reads the conversation again every 1.5 s while a question runs, so waits for an answer
// or a stop are bounded generously (the condition is what is awaited, not the time).
const POLL_WAIT = { timeout: 60_000 };
const SLOW_TEST_MS = 90_000;

function setUp(options: Parameters<FakeQuestionServer["add"]>[0] = {}, path?: (id: string) => string) {
  const server = new FakeQuestionServer();
  const conversation = server.add({
    title: "Energy this week",
    messages: [
      { kind: "question", text: "How did floor 3 use energy this week?" },
      { kind: "answer", text: "Floor 3 used 412 kWh." },
    ],
    ...options,
  });
  const rendered = renderAt(path ? path(conversation.id) : conversationPath(conversation.id), {
    conversations: server.api(),
  });
  return { server, conversation, ...rendered };
}

const box = () => screen.getByRole("textbox", { name: "Keep asking TrustIoT" }) as HTMLTextAreaElement;

async function typeAndSend(text: string) {
  const input = await screen.findByRole("textbox", { name: "Keep asking TrustIoT" });
  act(() => {
    fireEvent.change(input, { target: { value: text } });
  });
  act(() => {
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
  });
}

function thread() {
  return screen.getByRole("list", { name: "Questions and answers" });
}

describe("asking in a conversation", () => {
  test(
    "the question shows with a waiting line and a stop button, then the answer follows the earlier ones",
    async () => {
      const { server, conversation } = setUp();
      await typeAndSend("  And last week?  ");
      // Sent trimmed, as JSON `question`; the box is empty and the question is shown while it runs.
      await waitFor(() => expect(server.latestRun(conversation.id)?.status).toBe("running"));
      expect(server.requests.find((request) => request.url.pathname.endsWith("/questions"))?.body).toEqual({
        question: "And last week?",
      });
      const pending = await screen.findByRole("list", { name: "Question being answered" });
      expect(within(pending).getByText("And last week?")).toBeTruthy();
      expect(within(pending).getByText("Looking up readings…")).toBeTruthy();
      expect(box().value).toBe("");
      expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Send" })).toBeNull();

      act(() => server.answer(conversation.id, "Last week it was **441 kWh**."));
      await waitFor(
        () => expect(screen.queryByRole("list", { name: "Question being answered" })).toBeNull(),
        POLL_WAIT,
      );
      const items = within(thread())
        .getAllByRole("listitem")
        .filter((item) => item.parentElement === thread());
      expect(items.map((item) => (within(item).queryByRole("article") ? "answer" : "question"))).toEqual([
        "question",
        "answer",
        "question",
        "answer",
      ]);
      expect(screen.getAllByRole("article")[1]?.querySelector("strong")?.textContent).toBe("441 kWh");
      expect(screen.getByRole("button", { name: "Send" })).toBeTruthy();
      expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
    },
    SLOW_TEST_MS,
  );

  test("Enter sends, Shift + Enter breaks the line, and an Enter of an input method's composition sends nothing", async () => {
    const { server } = setUp();
    const input = await screen.findByRole("textbox", { name: "Keep asking TrustIoT" });
    act(() => {
      fireEvent.change(input, { target: { value: "line one" } });
    });
    act(() => {
      fireEvent.keyDown(input, { key: "Enter", shiftKey: true });
    });
    act(() => {
      fireEvent.keyDown(input, { key: "Enter", isComposing: true, keyCode: 229 });
    });
    expect(server.requests.filter((request) => request.url.pathname.endsWith("/questions"))).toHaveLength(0);
    act(() => {
      fireEvent.keyDown(input, { key: "Enter" });
    });
    await waitFor(() =>
      expect(server.requests.filter((request) => request.url.pathname.endsWith("/questions"))).toHaveLength(1),
    );
  });

  test("an empty question cannot be sent", async () => {
    const { server } = setUp();
    const input = await screen.findByRole("textbox", { name: "Keep asking TrustIoT" });
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
    act(() => {
      fireEvent.change(input, { target: { value: "   " } });
    });
    act(() => {
      fireEvent.keyDown(input, { key: "Enter" });
    });
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
    expect(server.requests.filter((request) => request.url.pathname.endsWith("/questions"))).toHaveLength(0);
  });

  test("while the question is on its way the button is off and the text stays", async () => {
    const { server } = setUp();
    server.holdAsks();
    await typeAndSend("Is it slow?");
    await waitFor(() =>
      expect(server.requests.some((request) => request.url.pathname.endsWith("/questions"))).toBe(true),
    );
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
    expect(box().value).toBe("Is it slow?");
    server.releaseAsks();
    await screen.findByRole("list", { name: "Question being answered" });
  });
});

describe("a question that cannot be sent", () => {
  test("keeps the typed text in the box and says why, for each way it can fail", async () => {
    const { server } = setUp();
    const cases: [Response, RegExp][] = [
      [fakeError(403, "permission_denied", "error.permission_denied"), /not sent:.*permission/i],
      [fakeError(429, "rate_limited", "question.running_limited", { max: 3 }), /already have 3 questions/],
      [fakeError(503, "unavailable", "question.busy"), /answering as many questions as it can/],
      [fakeError(409, "conflict", "question.history_full"), /too long to continue/],
    ];
    for (const [response, message] of cases) {
      server.failNextOn("ask", response);
      await typeAndSend("Keep this text");
      const alert = await screen.findByRole("alert");
      expect(alert.textContent).toMatch(message);
      expect(alert.textContent).toContain("Your text is still in the box.");
      expect(box().value).toBe("Keep this text");
      expect(screen.queryByRole("list", { name: "Question being answered" })).toBeNull();
    }
  });

  test("a question over the limit is refused in the service's words, with the text kept", async () => {
    const { server } = setUp();
    server.maxChars = 10;
    await typeAndSend("This is longer than ten characters");
    expect((await screen.findByRole("alert")).textContent).toContain("Enter a question of 1 to 10 characters.");
    expect(box().value).toBe("This is longer than ten characters");
  });

  test("no connection: the text stays and the message says so", async () => {
    const { server } = setUp();
    const fetch = server.fetch.bind(server);
    server.fetch = async (request) => {
      if (request.url.endsWith("/questions")) throw new TypeError("network down");
      return fetch(request);
    };
    await typeAndSend("Offline question");
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Your text is still in the box.");
    expect(box().value).toBe("Offline question");
  });

  test("a question already running (another tab) reads the conversation again and shows it running", async () => {
    const { server, conversation } = setUp();
    await screen.findByRole("textbox", { name: "Keep asking TrustIoT" });
    // Another tab asked first.
    server.failNextOn("ask", fakeError(409, "conflict", "conversation.question_running"));
    act(() => {
      server.runs.set("other-run", {
        id: "other-run",
        conversationId: conversation.id,
        text: "From another tab",
        status: "running",
        startedAt: "2026-11-04T01:00:00Z",
      });
    });
    await typeAndSend("My question");
    expect((await screen.findByRole("alert")).textContent).toContain("still being answered");
    // The page now follows the other question: the stop button, no send button.
    await screen.findByRole("button", { name: "Stop" });
  });
});

describe("stopping a question", () => {
  test(
    "says it is stopping, then that it stopped, with the question back in the box and no answer added",
    async () => {
      const { server, conversation } = setUp();
      await typeAndSend("A long question");
      await screen.findByRole("list", { name: "Question being answered" });
      act(() => {
        fireEvent.click(screen.getByRole("button", { name: "Stop" }));
      });
      await screen.findByRole("button", { name: "Stopping…" });
      const stopRequest = server.requests.find((request) => request.url.pathname.endsWith("/cancel"));
      expect(stopRequest?.method).toBe("POST");
      expect(server.latestRun(conversation.id)?.stopRequestedAt).toBeDefined();
      expect((screen.getByRole("button", { name: "Stopping…" }) as HTMLButtonElement).disabled).toBe(true);

      act(() => server.settleStop(conversation.id));
      await waitFor(
        () => expect(screen.queryByRole("list", { name: "Question being answered" })).toBeNull(),
        POLL_WAIT,
      );
      expect(screen.getByText(/Stopped\. Nothing of this question was kept/)).toBeTruthy();
      expect(box().value).toBe("A long question");
      // Nothing was added to the history, and asking works again.
      expect(within(thread()).getAllByRole("article")).toHaveLength(1);
      expect(screen.getByRole("button", { name: "Send" })).toBeTruthy();
    },
    SLOW_TEST_MS,
  );

  test("a stop that fails says why and leaves the question running", async () => {
    const { server } = setUp();
    await typeAndSend("A long question");
    await screen.findByRole("list", { name: "Question being answered" });
    server.failNextOn("stop", fakeError(503, "unavailable", "error.unavailable"));
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    });
    expect((await screen.findByRole("alert")).textContent).toContain("Could not stop the question");
    expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
  });

  test(
    "a stop that comes after the answer: the service says it ended, and the answer shows",
    async () => {
      const { server, conversation } = setUp();
      await typeAndSend("Quick question");
      await screen.findByRole("list", { name: "Question being answered" });
      act(() => server.answer(conversation.id, "Done already."));
      act(() => {
        fireEvent.click(screen.getByRole("button", { name: "Stop" }));
      });
      await waitFor(() => expect(screen.getAllByRole("article")).toHaveLength(2), POLL_WAIT);
      expect(screen.queryByRole("list", { name: "Question being answered" })).toBeNull();
    },
    SLOW_TEST_MS,
  );
});

describe("a question another tab replaced", () => {
  test(
    "the page stops following the older question and follows the newer one, so the composer is not stuck",
    async () => {
      const { server, conversation } = setUp();
      await typeAndSend("Mine");
      await screen.findByRole("list", { name: "Question being answered" });
      // This tab missed the end of its question; another tab asked a newer one.
      act(() => {
        server.interrupt(conversation.id);
        server.runs.set("newer-run", {
          id: "newer-run",
          conversationId: conversation.id,
          text: "From another tab",
          status: "running",
          startedAt: new Date(Date.now() + 60_000).toISOString(),
        });
      });
      // The newer question is followed: its text is not known here, only the waiting line.
      await waitFor(
        () =>
          expect(
            within(screen.getByRole("list", { name: "Question being answered" })).queryAllByRole("listitem"),
          ).toHaveLength(1),
        POLL_WAIT,
      );
      expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
      // When it ends, the composer is free again.
      act(() => {
        const newer = server.runs.get("newer-run");
        if (newer) newer.status = "answered";
      });
      await waitFor(() => expect(screen.getByRole("button", { name: "Send" })).toBeTruthy(), POLL_WAIT);
    },
    SLOW_TEST_MS,
  );
});

describe("a question that ends without an answer", () => {
  test(
    "shows the service's fixed message, keeps the history, and puts the question back",
    async () => {
      const { server, conversation } = setUp();
      await typeAndSend("Will fail");
      await screen.findByRole("list", { name: "Question being answered" });
      act(() => server.fail(conversation.id, "model_unavailable"));
      await waitFor(
        () =>
          expect(screen.getByRole("alert").textContent).toBe(
            "The assistant is unavailable right now. Please try again later.",
          ),
        POLL_WAIT,
      );
      expect(box().value).toBe("Will fail");
      expect(within(thread()).getAllByRole("article")).toHaveLength(1);
      // The next question goes as usual.
      act(() => {
        fireEvent.click(screen.getByRole("button", { name: "Send" }));
      });
      await screen.findByRole("list", { name: "Question being answered" });
      expect(screen.queryByRole("alert")).toBeNull();
    },
    SLOW_TEST_MS,
  );

  test(
    "an interrupted question says so; a key the page does not know gets the general message",
    async () => {
      const { server, conversation } = setUp();
      await typeAndSend("Will be interrupted");
      await screen.findByRole("list", { name: "Question being answered" });
      act(() => server.interrupt(conversation.id));
      await waitFor(
        () => expect(screen.getByRole("alert").textContent).toContain("interrupted because the assistant restarted"),
        POLL_WAIT,
      );
      const run = server.latestRun(conversation.id);
      expect(run?.status).toBe("interrupted");
      await typeAndSend("Another");
      await screen.findByRole("list", { name: "Question being answered" });
      act(() => {
        const next = server.latestRun(conversation.id);
        if (next) next.messageKey = "question.from_the_future";
        server.fail(conversation.id, "something_new");
        if (next) next.messageKey = "question.from_the_future";
      });
      await waitFor(
        () =>
          expect(screen.getByRole("alert").textContent).toBe(
            "The assistant could not answer this question. Please try again.",
          ),
        POLL_WAIT,
      );
    },
    SLOW_TEST_MS,
  );
});

describe("opening a conversation", () => {
  test("a question that is still running shows its waiting line and the stop button", async () => {
    const server = new FakeQuestionServer();
    const conversation = server.add({ title: "Energy" });
    server.runs.set("run-1", {
      id: "run-1",
      conversationId: conversation.id,
      text: "Started elsewhere",
      status: "running",
      startedAt: "2026-11-04T01:00:00Z",
    });
    renderAt(conversationPath(conversation.id), { conversations: server.api() });
    const pending = await screen.findByRole("list", { name: "Question being answered" });
    // Its text is not known here: only the waiting line.
    expect(within(pending).queryAllByRole("listitem")).toHaveLength(1);
    expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
  });

  test("a question named in the address is in the box, the cursor at its end, and the address is clean afterwards", async () => {
    const { router } = setUp({}, (id) => `${conversationPath(id)}?q=${encodeURIComponent("Change the title: ")}`);
    const input = await screen.findByRole("textbox", { name: "Keep asking TrustIoT" });
    expect((input as HTMLTextAreaElement).value).toBe("Change the title: ");
    await waitFor(() => expect(router.state.location.search).toBe(""));
    expect((input as HTMLTextAreaElement).value).toBe("Change the title: ");
  });

  test("a conversation with no questions yet says so, and can be asked in", async () => {
    const server = new FakeQuestionServer();
    const conversation = server.add({ title: "Empty", messages: [] });
    renderAt(conversationPath(conversation.id), { conversations: server.api() });
    expect(await screen.findByText("This conversation has no questions or answers yet.")).toBeTruthy();
    expect(screen.getByRole("textbox", { name: "Keep asking TrustIoT" })).toBeTruthy();
  });
});

describe("keeping the question being answered in view (Laysi/trustiot-v2#1182)", () => {
  /** Records every `scrollIntoView` with its element, and answers `prefers-reduced-motion` with `reduced`. */
  function watchScrolling(reduced: boolean) {
    const calls: { element: Element; options: ScrollIntoViewOptions | boolean | undefined }[] = [];
    const originalScroll = Element.prototype.scrollIntoView;
    const originalMedia = window.matchMedia;
    Element.prototype.scrollIntoView = function (this: Element, options?: ScrollIntoViewOptions | boolean) {
      calls.push({ element: this, options });
    };
    window.matchMedia = ((query: string) => ({
      matches: reduced && query === "(prefers-reduced-motion: reduce)",
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    })) as unknown as typeof window.matchMedia;
    return {
      calls,
      restore() {
        Element.prototype.scrollIntoView = originalScroll;
        window.matchMedia = originalMedia;
      },
    };
  }

  for (const reduced of [false, true]) {
    test(`a question sent is scrolled into view between the bars, ${reduced ? "at once for a reader who asked for less motion" : "smoothly"}`, async () => {
      const watch = watchScrolling(reduced);
      try {
        setUp();
        await typeAndSend("And last week?");
        const pending = await screen.findByRole("list", { name: "Question being answered" });
        await until(
          () => watch.calls.some((call) => call.element === pending),
          "the question being answered to be scrolled into view",
        );
        expect(watch.calls.filter((call) => call.element === pending).map((call) => call.options)).toEqual([
          { block: "nearest", behavior: reduced ? "instant" : "smooth" },
        ]);
      } finally {
        watch.restore();
      }
    });
  }

  test("the top bar and the composer publish their heights for the page's scroll-padding while the page is open", async () => {
    setUp();
    await screen.findByRole("textbox", { name: "Keep asking TrustIoT" });
    const root = document.documentElement.style;
    // happy-dom has no layout, so the heights are 0; that they are there is what is tested.
    expect(root.getPropertyValue("--sticky-top")).toMatch(/^\d+px$/);
    expect(root.getPropertyValue("--sticky-bottom")).toMatch(/^\d+px$/);
    cleanup();
    expect(root.getPropertyValue("--sticky-top")).toBe("");
    expect(root.getPropertyValue("--sticky-bottom")).toBe("");
  });
});

describe("deleting during a question", () => {
  test(
    "the dialog says the question will be stopped, and the delete stops it",
    async () => {
      const { server, conversation } = setUp();
      await typeAndSend("Long one");
      await screen.findByRole("list", { name: "Question being answered" });
      // The page knows a question runs once it has read the conversation again.
      await waitFor(() => expect(server.latestRun(conversation.id)?.status).toBe("running"));
      const trigger = await screen.findByRole("button", { name: "Conversation actions" });
      await act(async () => {
        fireEvent.keyDown(trigger, { key: "Enter" });
      });
      act(() => {
        fireEvent.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: "Delete" }));
      });
      const dialog = await screen.findByRole("alertdialog");
      await waitFor(() => expect(dialog.textContent).toContain("deleting stops it"));
      act(() => {
        fireEvent.click(within(dialog).getByRole("button", { name: "Delete conversation" }));
      });
      await waitFor(() => expect(server.conversations.get(conversation.id)?.deleted_at).toBeDefined());
      expect(server.latestRun(conversation.id)?.status).toBe("cancelled");
    },
    SLOW_TEST_MS,
  );
});
