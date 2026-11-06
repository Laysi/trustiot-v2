import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, within } from "@testing-library/react";
import { Outlet, type RouteObject } from "react-router";

import { conversationPath, sharedConversationPath } from "../../app/paths.ts";
import { ROUTES } from "../../app/routes.tsx";
import { fakeError } from "../../test/fake-dashboards.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { regenerateFor } from "../../test/fake-regenerate.ts";
import { renderAt } from "../../test/render.tsx";
import { until } from "../../test/until.ts";
import { RegenerateAvailable } from "./regenerate.tsx";

// Regenerating the last answer (Laysi/trustiot-v2#1047), against the question fake that follows
// the pending endpoint of Laysi/trustiot-v2#649 (PR Laysi/trustiot-v2#1083).

afterEach(() => {
  cleanup();
});

const Q1 = "How did floor 3 use energy this week?";
const A1 = "Floor 3 used 412 kWh.";
const Q2 = "And last week?";
const A2 = "Last week it was 441 kWh.";
const FRESH = "Last week it was 441 kWh, a bit more.";

/** The app's routes with the regenerate endpoint provided, as the UI preview does. */
function withRegenerate(server: FakeQuestionServer): RouteObject[] {
  return [
    {
      element: (
        <RegenerateAvailable api={regenerateFor(server)}>
          <Outlet />
        </RegenerateAvailable>
      ),
      children: ROUTES,
    },
  ];
}

/** A conversation of two answered rounds; the endpoint is provided unless `provided` is false. */
function setUp(provided = true, messages?: Parameters<FakeQuestionServer["add"]>[0]) {
  const server = new FakeQuestionServer();
  const conversation = server.add({
    title: "Energy this week",
    messages: [
      { kind: "question", text: Q1 },
      { kind: "answer", text: A1 },
      { kind: "question", text: Q2 },
      { kind: "answer", text: A2 },
    ],
    ...messages,
  });
  const rendered = renderAt(conversationPath(conversation.id), {
    conversations: server.api(),
    ...(provided ? { routes: withRegenerate(server) } : {}),
  });
  return { server, conversation, ...rendered };
}

const regenerateButton = () => screen.queryByRole("button", { name: /^Regenerate/ });
const composer = () => screen.getByRole("textbox", { name: "Keep asking TrustIoT" }) as HTMLTextAreaElement;

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

/** The thread's answers and questions, in order, as text. */
function shown(): string[] {
  const list = screen.getByRole("list", { name: "Questions and answers" });
  return Array.from(list.children).map(
    (item) => item.textContent?.replace(/^You asked:/, "").replace(/Copy answer.*$/, "") ?? "",
  );
}

/** Waits for the page to have read the conversation and drawn its answers. */
async function loaded() {
  await screen.findAllByRole("article", { name: "TrustIoT's answer" });
}

/** The regenerate requests the fake got, as their bodies. */
function regenerations(server: FakeQuestionServer): unknown[] {
  return server.requests.filter((request) => request.url.pathname.endsWith("/regenerate")).map((r) => r.body);
}

function questionsAsked(server: FakeQuestionServer): unknown[] {
  return server.requests.filter((request) => request.url.pathname.endsWith("/questions")).map((r) => r.body);
}

describe("the regenerate button", () => {
  test("is on the last answer only, in the row with the copy button", async () => {
    setUp();
    await loaded();
    const answers = screen.getAllByRole("article", { name: "TrustIoT's answer" });
    expect(answers).toHaveLength(2);
    expect(within(answers[0] as HTMLElement).queryByRole("button", { name: /Regenerate/ })).toBeNull();
    const last = answers[1] as HTMLElement;
    expect(within(last).getByRole("button", { name: "Regenerate" })).toBeTruthy();
    expect(within(last).getByRole("button", { name: "Copy answer" })).toBeTruthy();
    expect(screen.getAllByRole("button", { name: "Regenerate" })).toHaveLength(1);
  });

  test("is not there where the build has no regenerate endpoint (the production default)", async () => {
    setUp(false);
    await loaded();
    expect(regenerateButton()).toBeNull();
  });

  test("is not on a conversation that has no answer, or whose last round has none", async () => {
    setUp(true, { messages: [] });
    expect(await screen.findByText("This conversation has no questions or answers yet.")).toBeTruthy();
    expect(regenerateButton()).toBeNull();
  });

  test("a shared conversation's read-only page never has it, even where the endpoint is provided", async () => {
    const server = new FakeQuestionServer();
    const owner = "00000000-0000-4000-8000-00000000b0b0";
    const shared = server.add({
      title: "Boiler room energy",
      owner,
      sharedAt: "2026-11-02T02:00:00Z",
      messages: [
        { kind: "question", text: "How much energy did the boiler room use yesterday?" },
        { kind: "answer", text: "88 kWh." },
      ],
    });
    renderAt(sharedConversationPath(shared.id), { conversations: server.api(), routes: withRegenerate(server) });
    await screen.findByRole("article", { name: "TrustIoT's answer" });
    expect(regenerateButton()).toBeNull();
    // The read of a shared conversation carries no positions: there is nothing to name an answer by.
    expect(JSON.stringify(server.requests.map((request) => request.url.pathname))).not.toContain("regenerate");
  });
});

describe("regenerating", () => {
  test("names the last answer's position, shows the question being answered, then the new answer replaces the old one", async () => {
    const { server, conversation } = setUp();
    await loaded();
    const position = server.seqOf(conversation.id, 3);
    click(screen.getByRole("button", { name: "Regenerate" }));
    await until(() => server.latestRun(conversation.id)?.status === "running", "the regenerated question to run");
    expect(regenerations(server)).toEqual([{ answer: position }]);

    // The old round is gone from the first moment, and the question is shown as being answered.
    const pending = await screen.findByRole("list", { name: "Question being answered" });
    expect(within(pending).getByText(Q2)).toBeTruthy();
    expect(within(pending).getByText("Looking up readings…")).toBeTruthy();
    await until(() => !shown().join("|").includes(A2), "the old answer to leave the thread");
    expect(shown()).toEqual([Q1, A1]);

    act(() => server.answer(conversation.id, FRESH));
    await until(() => shown().join("|").includes(FRESH), "the new answer");
    expect(shown()).toEqual([Q1, A1, Q2, FRESH]);
    expect(server.conversations.get(conversation.id)?.messages.map((message) => message.text)).toEqual([
      Q1,
      A1,
      Q2,
      FRESH,
    ]);

    // The new answer has a new position, and a second regeneration names that one.
    await until(() => regenerateButton() !== null, "the button on the new answer");
    const next = server.seqOf(conversation.id, 3);
    expect(next).not.toBe(position);
    click(screen.getByRole("button", { name: "Regenerate" }));
    await until(() => regenerations(server).length === 2, "the second regeneration");
    expect(regenerations(server)[1]).toEqual({ answer: next });
  });

  test("cannot be pressed twice while it is sent, and is neither visible nor usable while an answer is written", async () => {
    const { server, conversation } = setUp();
    await loaded();
    server.holdAsks();
    click(screen.getByRole("button", { name: "Regenerate" }));
    const pressed = await screen.findByRole("button", { name: "Regenerating…" });
    expect((pressed as HTMLButtonElement).disabled).toBe(true);
    click(pressed);
    server.releaseAsks();
    await until(
      () => screen.queryByRole("list", { name: "Question being answered" }) !== null,
      "the question to be answered",
    );
    expect(regenerations(server)).toHaveLength(1);
    // Being answered: no button at all, not even disabled.
    expect(regenerateButton()).toBeNull();
    expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
    act(() => server.answer(conversation.id, FRESH));
    await until(() => regenerateButton() !== null, "the button after the answer");
  });

  test("is hidden while a question of the user's own is answered, and comes back with its answer", async () => {
    const { server, conversation } = setUp();
    await loaded();
    act(() => {
      fireEvent.change(composer(), { target: { value: "Compare floors" } });
    });
    click(screen.getByRole("button", { name: "Send" }));
    await screen.findByRole("list", { name: "Question being answered" });
    expect(regenerateButton()).toBeNull();
    act(() => server.answer(conversation.id, "Floor 2 used less."));
    await until(() => regenerateButton() !== null, "the button on the new last answer");
    expect(
      within(screen.getAllByRole("article", { name: "TrustIoT's answer" })[2] as HTMLElement).queryByRole("button", {
        name: "Regenerate",
      }),
    ).toBeTruthy();
  });

  test("is hidden when a question is already running as the page opens (another tab's)", async () => {
    const server = new FakeQuestionServer();
    const conversation = server.add({
      messages: [
        { kind: "question", text: Q1 },
        { kind: "answer", text: A1 },
      ],
    });
    await server.api().askQuestion(conversation.id, Q2);
    renderAt(conversationPath(conversation.id), { conversations: server.api(), routes: withRegenerate(server) });
    await loaded();
    expect(regenerateButton()).toBeNull();
  });
});

describe("a stale position", () => {
  test("409 question.regenerate_stale reads the conversation again, shows how it is now, and never sends again", async () => {
    const { server, conversation } = setUp();
    await loaded();
    // Another tab regenerated the answer and the new one arrived; this page still shows the old one.
    const staleSeq = server.seqOf(conversation.id, 3);
    await regenerateFor(server).regenerate(conversation.id, staleSeq);
    act(() => server.answer(conversation.id, FRESH));
    expect(shown().join("|")).toContain(A2);

    click(screen.getByRole("button", { name: "Regenerate" }));
    const notice = await screen.findByText(/This conversation changed since you opened it/);
    expect(notice.closest("[role=status]")).toBeTruthy();
    // The page shows what the service holds now, and nothing was discarded by this press.
    await until(() => shown().join("|").includes(FRESH) && !shown().join("|").includes(A2), "the current conversation");
    expect(shown()).toEqual([Q1, A1, Q2, FRESH]);
    // The other tab's request and this page's one stale press: the page never sends again with the
    // new position by itself (it had the time to, while it read the conversation and drew it).
    await new Promise((resume) => setTimeout(resume, 300));
    const bodies = regenerations(server);
    expect(bodies).toEqual([{ answer: staleSeq }, { answer: staleSeq }]);
    expect(server.conversations.get(conversation.id)?.messages).toHaveLength(4);
    // The button on the new last answer is the user's to press, with the new position.
    const current = server.seqOf(conversation.id, 3);
    expect(current).not.toBe(staleSeq);
    click(screen.getByRole("button", { name: "Regenerate" }));
    await until(() => regenerations(server).length === 3, "the user's own press");
    expect(regenerations(server)[2]).toEqual({ answer: current });
  });
});

describe("after a regeneration that fails or is stopped", () => {
  test("the original question goes back into the box and no second regeneration is offered", async () => {
    const { server, conversation } = setUp();
    await loaded();
    click(screen.getByRole("button", { name: "Regenerate" }));
    await screen.findByRole("list", { name: "Question being answered" });
    act(() => server.fail(conversation.id));
    await until(
      () => screen.queryByRole("list", { name: "Question being answered" }) === null,
      "the failed run to end",
    );

    // The conversation stays at the round before: the old answer does not come back.
    expect(shown()).toEqual([Q1, A1]);
    expect(composer().value).toBe(Q2);
    expect(screen.getByRole("alert").textContent).toContain("The assistant is unavailable right now.");
    expect(screen.getByRole("alert").textContent).toContain("The answer you asked to replace was removed");
    // The last answer on the page is an earlier round's: regenerating it would discard that round.
    expect(regenerateButton()).toBeNull();
    expect(regenerations(server)).toHaveLength(1);
  });

  test("a stopped regeneration says so, and offers asking again, not regenerating", async () => {
    const { server, conversation } = setUp();
    await loaded();
    click(screen.getByRole("button", { name: "Regenerate" }));
    await screen.findByRole("list", { name: "Question being answered" });
    click(screen.getByRole("button", { name: "Stop" }));
    await until(() => server.latestRun(conversation.id)?.stopRequestedAt !== undefined, "the stop request");
    act(() => server.settleStop(conversation.id));
    await until(() => composer().value === Q2, "the question back in the box");
    expect(screen.getByText(/The answer you asked to replace was removed too/)).toBeTruthy();
    expect(shown()).toEqual([Q1, A1]);
    expect(regenerateButton()).toBeNull();
  });

  test("asking the question again is a new question, and its answer brings the button back for that answer", async () => {
    const { server, conversation } = setUp();
    await loaded();
    click(screen.getByRole("button", { name: "Regenerate" }));
    await screen.findByRole("list", { name: "Question being answered" });
    act(() => server.fail(conversation.id));
    await until(() => composer().value === Q2, "the question back in the box");

    click(screen.getByRole("button", { name: "Send" }));
    await until(() => questionsAsked(server).length === 1, "the new question");
    expect(questionsAsked(server)).toEqual([{ question: Q2 }]);
    // Still no regenerate while it is answered, nor a second one for the same turn.
    expect(regenerateButton()).toBeNull();
    act(() => server.answer(conversation.id, FRESH));
    await until(() => regenerateButton() !== null, "the button on the new answer");
    expect(shown()).toEqual([Q1, A1, Q2, FRESH]);
    expect(regenerations(server)).toHaveLength(1);
  });

  test("a refusal that comes after the service discarded the round puts the question back and offers no regeneration", async () => {
    const { server, conversation } = setUp();
    await loaded();
    // The service discards the round, then the identity service refuses the question (503): the
    // response is an error, but the answer is gone.
    server.failNextOn("regenerate", fakeError(503, "unavailable", "question.busy"));
    const messages = server.conversations.get(conversation.id)?.messages ?? [];
    click(screen.getByRole("button", { name: "Regenerate" }));
    messages.splice(2, 2);
    await screen.findByText(/The old answer was already removed/);
    expect(composer().value).toBe(Q2);
    expect(shown()).toEqual([Q1, A1]);
    expect(regenerateButton()).toBeNull();
  });

  test("a refusal before any discard leaves the answer and the button, with the reason", async () => {
    const { server } = setUp();
    await loaded();
    server.failNextOn("regenerate", fakeError(503, "unavailable", "question.busy"));
    click(screen.getByRole("button", { name: "Regenerate" }));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Couldn't regenerate the answer:");
    expect(alert.textContent).toContain("The assistant is answering as many questions as it can right now.");
    expect(alert.textContent).not.toContain("removed");
    expect(shown()).toEqual([Q1, A1, Q2, A2]);
    expect(composer().value).toBe("");
    await until(
      () => regenerateButton() !== null && !(regenerateButton() as HTMLButtonElement).disabled,
      "the button to be usable again",
    );
  });
});
