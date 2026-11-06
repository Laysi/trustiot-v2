import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath } from "../../app/paths.ts";
import { fakeError } from "../../test/fake-dashboards.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { memoryPreferences, renderAt, signedInBrowser } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

const POLL_WAIT = { timeout: 60_000 };
const SLOW_TEST_MS = 90_000;

const box = () => screen.getByRole("textbox", { name: "Ask TrustIoT" }) as HTMLTextAreaElement;

async function typeAndSend(text: string) {
  const input = await screen.findByRole("textbox", { name: "Ask TrustIoT" });
  act(() => {
    fireEvent.change(input, { target: { value: text } });
  });
  act(() => {
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
  });
}

describe("the home page's composer", () => {
  test(
    "asking starts a conversation, asks in it and opens its page, which shows the question and then the answer",
    async () => {
      const server = new FakeQuestionServer();
      const { router } = renderAt("/", { conversations: server.api() });
      await typeAndSend("How much energy did tower 2 use?");
      await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\/[0-9a-f-]{36}$/));
      const [conversation] = [...server.conversations.values()];
      expect(router.state.location.pathname).toBe(conversationPath(conversation?.id ?? ""));
      expect(server.requests.map((request) => `${request.method} ${request.url.pathname}`)).toContain(
        `POST /api/assistant/v1/conversations/${conversation?.id}/questions`,
      );
      // The question is shown while it is answered.
      const pending = await screen.findByRole("list", { name: "Question being answered" });
      expect(within(pending).getByText("How much energy did tower 2 use?")).toBeTruthy();

      act(() => server.answer(conversation?.id ?? "", "Tower 2 used **412 kWh**."));
      await waitFor(() => expect(screen.getAllByRole("article")).toHaveLength(1), POLL_WAIT);
      expect(screen.getByText("412 kWh")).toBeTruthy();
      const thread = screen.getByRole("list", { name: "Questions and answers" });
      expect(within(thread).getByText("How much energy did tower 2 use?")).toBeTruthy();
    },
    SLOW_TEST_MS,
  );

  test("a question that cannot be sent leaves the text in the box and says why; the retry reuses the conversation started", async () => {
    const server = new FakeQuestionServer();
    const { router } = renderAt("/", { conversations: server.api() });
    server.failNextOn("ask", fakeError(429, "rate_limited", "question.running_limited", { max: 3 }));
    await typeAndSend("Keep this");
    expect((await screen.findByRole("alert")).textContent).toContain("already have 3 questions");
    expect(box().value).toBe("Keep this");
    expect(router.state.location.pathname).toBe("/");
    expect(server.conversations.size).toBe(1);

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Send" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    // Still one conversation: the retry only asked.
    expect(server.conversations.size).toBe(1);
  });

  test("the conversation starts with the user's AI language and time zone, `auto` as the browser's zone", async () => {
    const server = new FakeQuestionServer();
    // The same preferences on the server and in the browser's copy, whichever applies when sending.
    const preferences = { language: "en", aiLanguage: "zh-TW", timeZone: "auto" } as const;
    const { router } = renderAt("/", {
      conversations: server.api(),
      identity: signedInBrowser({ preferences }).browser,
      preferences: memoryPreferences(preferences),
      browserZone: "Europe/Berlin",
    });
    await typeAndSend("Energy this week?");
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    const creates = server.requests.filter(
      (request) => request.method === "POST" && request.url.pathname === "/api/assistant/v1/conversations",
    );
    expect(creates.map((request) => request.body)).toEqual([{ ai_language: "zh-TW", time_zone: "Europe/Berlin" }]);
  });

  test("starting the conversation can fail too, with the text kept", async () => {
    const server = new FakeQuestionServer();
    renderAt("/", { conversations: server.api() });
    server.failNextOn("create", fakeError(503, "unavailable", "error.unavailable"));
    await typeAndSend("Keep this too");
    expect((await screen.findByRole("alert")).textContent).toContain("Your text is still in the box.");
    expect(box().value).toBe("Keep this too");
    expect(server.conversations.size).toBe(0);
  });

  test("while sending, the status says what is going on and the button is off", async () => {
    const server = new FakeQuestionServer();
    const { router } = renderAt("/", { conversations: server.api() });
    server.holdAsks();
    await typeAndSend("Slow one");
    expect(await screen.findByText("Looking up readings…")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
    expect(router.state.location.pathname).toBe("/");
    server.releaseAsks();
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
  });

  test("a question named in the address is in the box (suggested text, not sent), and the address is clean", async () => {
    const server = new FakeQuestionServer();
    const { router } = renderAt(`/?q=${encodeURIComponent('Change the dashboard "Energy": ')}`, {
      conversations: server.api(),
    });
    const input = await screen.findByRole("textbox", { name: "Ask TrustIoT" });
    expect((input as HTMLTextAreaElement).value).toBe('Change the dashboard "Energy": ');
    await waitFor(() => expect(router.state.location.search).toBe(""));
    expect(server.requests.filter((request) => request.method === "POST")).toHaveLength(0);
  });

  test("markup in the address is text in the box, never elements", async () => {
    const server = new FakeQuestionServer();
    renderAt(`/?q=${encodeURIComponent("<img src=x onerror=alert(1)>")}`, { conversations: server.api() });
    const input = await screen.findByRole("textbox", { name: "Ask TrustIoT" });
    expect((input as HTMLTextAreaElement).value).toBe("<img src=x onerror=alert(1)>");
    expect(document.querySelector("img[src='x']")).toBeNull();
  });
});
