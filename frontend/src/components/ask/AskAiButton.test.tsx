import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { ReactNode } from "react";
import type { RouteObject } from "react-router";

import { Layout } from "../../app/Layout.tsx";
import { conversationPath } from "../../app/paths.ts";
import { ConversationPage } from "../../pages/conversations/ConversationPage.tsx";
import { TenantGate } from "../../session/TenantGate.tsx";
import { fakeError } from "../../test/fake-dashboards.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { renderAt } from "../../test/render.tsx";
import { AskAiButton } from "./AskAiButton.tsx";

afterEach(() => {
  cleanup();
});

/** The conversation page after the question, so a test sees where the button leads. */
function routesWith(probe: ReactNode): RouteObject[] {
  return [
    {
      path: "/",
      element: <Layout />,
      children: [
        {
          element: <TenantGate />,
          children: [
            { index: true, element: <p>The home page</p> },
            { path: "probe", element: probe },
            { path: "conversations/:conversationId", element: <ConversationPage /> },
          ],
        },
      ],
    },
  ];
}

function open(probe: ReactNode, path = "/probe") {
  const server = new FakeQuestionServer();
  const rendered = renderAt(path, { routes: routesWith(probe), conversations: server.api() });
  return { server, ...rendered };
}

const asked = (server: FakeQuestionServer) =>
  server.requests.filter((request) => request.method === "POST" && request.url.pathname.endsWith("/questions"));

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

describe("the 「Ask AI」 button outside a conversation", () => {
  test("starts a conversation, asks the question as it is, and opens the conversation", async () => {
    const { server, router } = open(<AskAiButton question="Which devices are offline?" />);
    click(await screen.findByRole("button", { name: "Which devices are offline?" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\/[0-9a-f-]{36}$/));
    const [conversation] = [...server.conversations.values()];
    expect(router.state.location.pathname).toBe(conversationPath(conversation?.id ?? ""));
    expect(asked(server).map((request) => request.body)).toEqual([{ question: "Which devices are offline?" }]);
    // The conversation page follows the question it was opened with.
    const pending = await screen.findByRole("list", { name: "Question being answered" });
    expect(within(pending).getByText("Which devices are offline?")).toBeTruthy();
  });

  test("sends the full question while the button says less", async () => {
    const { server, router } = open(<AskAiButton label="Why offline?" question="Why is Compressor A2 offline?" />);
    click(await screen.findByRole("button", { name: "Why offline?" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    expect(asked(server).map((request) => request.body)).toEqual([{ question: "Why is Compressor A2 offline?" }]);
  });

  test("writes what the question is about after it", async () => {
    const { server, router } = open(
      <AskAiButton question="Why did it fire?" context={["Rule: Overheat", "Device: Boiler B1"]} />,
    );
    click(await screen.findByRole("button", { name: "Why did it fire?" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    expect(asked(server).map((request) => request.body)).toEqual([
      { question: "Why did it fire? (Rule: Overheat, Device: Boiler B1)" },
    ]);
  });

  test("with a page to lead back to, the conversation opens with a link back to the page as it was", async () => {
    const { router } = open(<AskAiButton question="Compare?" back={{ label: "Compressor A2" }} />, "/probe?range=7d");
    click(await screen.findByRole("button", { name: "Compare?" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    const back = await screen.findByRole("link", { name: "Back to Compressor A2" });
    expect(back.getAttribute("href")).toBe("/probe?range=7d");
    click(back);
    await waitFor(() => expect(router.state.location.pathname).toBe("/probe"));
    expect(router.state.location.search).toBe("?range=7d");
  });

  test("without a page to lead back to, the conversation has no such link", async () => {
    const { router } = open(<AskAiButton question="Compare?" />);
    click(await screen.findByRole("button", { name: "Compare?" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    await screen.findByRole("list", { name: "Question being answered" });
    expect(screen.queryByRole("link", { name: /^Back to / })).toBeNull();
  });

  test("a link back that names another site is not followed", async () => {
    const { router, server } = open(<AskAiButton question="Compare?" />);
    const conversation = server.add({ title: "Energy", messages: [] });
    await screen.findByRole("button", { name: "Compare?" });
    await act(async () => {
      await router.navigate(conversationPath(conversation.id), {
        state: { askedFrom: { to: "//other.example/phish", label: "Compressor A2" } },
      });
    });
    await screen.findByRole("heading", { level: 1, name: "Energy" });
    expect(screen.queryByRole("link", { name: /^Back to / })).toBeNull();
  });

  test("while it waits the button stays focusable and a second click asks nothing", async () => {
    const { server, router } = open(<AskAiButton question="Slow one" />);
    server.holdAsks();
    const button = await screen.findByRole("button", { name: "Slow one" });
    click(button);
    await waitFor(() => expect(button.getAttribute("aria-disabled")).toBe("true"));
    expect(screen.getByText("Sending the question…")).toBeTruthy();
    click(button);
    server.releaseAsks();
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    expect(asked(server)).toHaveLength(1);
  });

  describe("when the question cannot be sent", () => {
    // A suspended organization is not here: the tenant gate replaces the whole page then.
    const refusals = [
      [
        "the quota is used up",
        fakeError(403, "quota_exceeded", "error.quota_exceeded"),
        "Your plan's limit has been reached.",
      ],
      [
        "there is no permission",
        fakeError(403, "permission_denied", "error.permission_denied"),
        "You don't have permission to do this.",
      ],
      [
        "the service is paused",
        fakeError(503, "unavailable", "error.unavailable"),
        "The service is temporarily unavailable.",
      ],
      [
        "the assistant is at its limit",
        fakeError(503, "unavailable", "question.busy"),
        "The assistant is answering as many questions as it can right now.",
      ],
    ] as const;

    for (const [when, response, reason] of refusals) {
      test(`the reason stands under the button when ${when}, tied to it and read out`, async () => {
        const { server, router } = open(<AskAiButton question="Which devices are offline?" />);
        server.failNextOn("ask", response);
        const button = await screen.findByRole("button", { name: "Which devices are offline?" });
        click(button);
        const alert = await screen.findByRole("alert");
        expect(alert.textContent).toContain("The question wasn't sent:");
        expect(alert.textContent).toContain(reason);
        expect(button.getAttribute("aria-describedby")).toBe(alert.id);
        // The alert is the button's neighbour, under it, and the page stays where it was.
        expect(button.parentElement).toBe(alert.parentElement);
        expect(router.state.location.pathname).toBe("/probe");
      });
    }

    test("the button works again, and a retry uses the conversation already started", async () => {
      const { server, router } = open(<AskAiButton question="Which devices are offline?" />);
      server.failNextOn("ask", fakeError(503, "unavailable", "error.unavailable"));
      click(await screen.findByRole("button", { name: "Which devices are offline?" }));
      await screen.findByRole("alert");
      expect(server.conversations.size).toBe(1);

      click(screen.getByRole("button", { name: "Which devices are offline?" }));
      await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
      expect(server.conversations.size).toBe(1);
      expect(asked(server)).toHaveLength(2);
    });

    test("starting the conversation can fail too", async () => {
      const { server } = open(<AskAiButton question="Which devices are offline?" />);
      server.failNextOn("create", fakeError(503, "unavailable", "error.unavailable"));
      click(await screen.findByRole("button", { name: "Which devices are offline?" }));
      expect((await screen.findByRole("alert")).textContent).toContain("The question wasn't sent:");
      expect(server.conversations.size).toBe(0);
    });
  });
});
