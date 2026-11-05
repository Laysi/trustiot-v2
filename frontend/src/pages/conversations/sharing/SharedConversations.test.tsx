import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, within } from "@testing-library/react";

import { conversationPath, sharedConversationPath, sharedConversationsPath } from "../../../app/paths.ts";
import { DAILY_ROWS, SPECS } from "../../../dashboard/fixtures.ts";
import { dashboardFormat } from "../../../dashboard/format.ts";
import { formatDateTime } from "../../../i18n/format.ts";
import { fakeEngine } from "../../../test/fake-chart.ts";
import { FakeConversationServer } from "../../../test/fake-conversations.ts";
import { FakeDashboardServer, type FakeRole, fakeError } from "../../../test/fake-dashboards.ts";
import { fakeUuid } from "../../../test/fake-devices.ts";
import { FakeQuestionServer } from "../../../test/fake-questions.ts";
import { renderAt, signedInBrowser } from "../../../test/render.tsx";
import { LONG, until } from "../../../test/until.ts";

afterEach(() => {
  cleanup();
});

/** Another member of the tenant. */
const BOB = fakeUuid();
const SHARED_AT = "2026-11-02T02:00:00Z";
const when = (iso: string) => formatDateTime(iso, "en", "UTC");

const ANSWER = [
  "The boiler room used **88 kWh** yesterday.",
  "",
  "![plan](https://evil.example.test/p.png) [report](https://evil.example.test/r)",
].join("\n");

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

/** A server where Bob shared one conversation and kept another to himself. */
function setUp() {
  const server = new FakeConversationServer();
  const shared = server.add({
    title: "Boiler room energy",
    owner: BOB,
    sharedAt: SHARED_AT,
    activityAt: "2026-11-03T05:00:00Z",
    messages: [
      { kind: "question", text: "How much energy did the boiler room use yesterday?" },
      { kind: "answer", text: ANSWER },
    ],
  });
  const private_ = server.add({ title: "Bob's own notes", owner: BOB });
  return { server, shared, private_ };
}

function openAt(server: FakeConversationServer, path: string, options: Parameters<typeof renderAt>[1] = {}) {
  return renderAt(path, { conversations: server.api(), ...options });
}

/** The titles in the table, in order. */
function listedTitles(): string[] {
  return within(screen.getByRole("table"))
    .getAllByRole("rowheader")
    .map((cell) => cell.textContent?.trim() ?? "");
}

describe("the Shared with me tab", () => {
  test("is the history's fourth tab, and lists what other members shared, never what they kept", async () => {
    const { server, shared } = setUp();
    openAt(server, "/conversations");
    expect(await screen.findByText("No conversations yet")).toBeTruthy();
    click(screen.getByRole("button", { name: "Shared with me" }));

    expect(await screen.findByRole("table")).toBeTruthy();
    expect(listedTitles()).toEqual(["Boiler room energy"]);
    const link = screen.getByRole("link", { name: "Boiler room energy" });
    expect(link.getAttribute("href")).toBe(sharedConversationPath(shared.id));
    const row = link.closest("tr") as HTMLElement;
    expect(within(row).getByText("Another member")).toBeTruthy();
    expect(within(row).getByText(when(SHARED_AT))).toBeTruthy();
    expect(within(row).getByText(when("2026-11-03T05:00:00Z"))).toBeTruthy();
    // Only reading: there is no action on a row.
    expect(within(row).queryByRole("button")).toBeNull();
    expect(screen.getByRole("note").textContent).toContain("You can only read these conversations");
    expect(screen.getByText(/Conversations other members shared with everyone in this organization/)).toBeTruthy();
  });

  test("a deep link to ?tab=shared opens it, with the tab marked", async () => {
    const { server } = setUp();
    openAt(server, sharedConversationsPath());
    expect(await screen.findByRole("table")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Shared with me" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "In use" }).getAttribute("aria-pressed")).toBe("false");
  });

  test("the user's own shared conversation is in the other tabs, not in this list", async () => {
    const server = new FakeConversationServer();
    server.add({ title: "Mine, shared", sharedAt: SHARED_AT });
    openAt(server, sharedConversationsPath());
    expect(await screen.findByText("Nothing shared with you yet")).toBeTruthy();
  });

  test("says so when nothing is shared, and when a search finds nothing", async () => {
    const { server } = setUp();
    server.conversations.forEach((item) => {
      delete item.shared_at;
    });
    openAt(server, sharedConversationsPath());
    expect(await screen.findByText("Nothing shared with you yet")).toBeTruthy();
    expect(screen.getByText(/When another member shares a conversation with the organization/)).toBeTruthy();
  });

  test(
    "searches titles, reading the pages that are not shown yet",
    async () => {
      const server = new FakeConversationServer();
      for (let index = 1; index <= 120; index += 1) {
        server.add({
          title: index === 1 ? "Oldest boiler" : `Shared ${index}`,
          owner: BOB,
          sharedAt: SHARED_AT,
          activityAt: new Date(Date.parse("2026-10-01T00:00:00Z") + index * 60_000).toISOString(),
        });
      }
      openAt(server, sharedConversationsPath());
      expect(await screen.findByRole("table")).toBeTruthy();
      expect(listedTitles()).toHaveLength(100);
      expect(screen.getByRole("button", { name: "Show more" })).toBeTruthy();
      // The oldest is on the second page: the search reads it.
      act(() => {
        fireEvent.change(screen.getByRole("searchbox", { name: "Search conversations" }), {
          target: { value: " oldest " },
        });
      });
      await until(() => document.querySelectorAll("tbody tr").length === 1, "the search to read the second page");
      expect(listedTitles()).toEqual(["Oldest boiler"]);
    },
    LONG + 10_000,
  );

  test("an error shows with a retry that reads the list again", async () => {
    const { server } = setUp();
    server.failNext("listShared", fakeError(503, "unavailable", "error.unavailable"));
    openAt(server, sharedConversationsPath());
    expect(await screen.findByText(/Couldn't load the shared conversations:/)).toBeTruthy();
    click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("table")).toBeTruthy();
  });

  test("shows a skeleton while it loads", async () => {
    const { server } = setUp();
    server.hold(["listShared"]);
    openAt(server, sharedConversationsPath());
    expect(await screen.findByText("Loading shared conversations…")).toBeTruthy();
    server.release();
    expect(await screen.findByRole("table")).toBeTruthy();
  });
});

describe("a shared conversation's page", () => {
  test("is read only: the title, the times, the questions and answers, and nothing of the owner's", async () => {
    const { server, shared } = setUp();
    openAt(server, sharedConversationPath(shared.id));
    expect(await screen.findByRole("heading", { level: 1, name: "Boiler room energy" })).toBeTruthy();
    expect(screen.getByText("Read only")).toBeTruthy();
    expect(screen.getByText(`Shared ${when(SHARED_AT)} · Last activity ${when("2026-11-03T05:00:00Z")}`)).toBeTruthy();

    const thread = screen.getByRole("list", { name: "Questions and answers" });
    expect(thread.textContent).toContain("How much energy did the boiler room use yesterday?");
    const answer = screen.getByRole("article", { name: "TrustIoT's answer" });
    // The same safe Markdown as the owner's page: no image request, no external link.
    expect(answer.querySelector("strong")?.textContent).toBe("88 kWh");
    expect(answer.querySelector("img")).toBeNull();
    expect(answer.querySelector("a")).toBeNull();
    expect(answer.textContent).toContain("report (https://evil.example.test/r)");

    // Nothing the owner can do: no composer, stop, regenerate, rename, archive, delete, share, save.
    expect(screen.queryByRole("textbox")).toBeNull();
    for (const name of [
      /^Share/,
      /^Stop/,
      /Regenerate/i,
      /^Rename/,
      /Archive/,
      /^Delete/,
      /^Save/,
      /^Publish/,
      "Conversation actions",
    ]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    // What it offers: copying, and the note on what a copy holds and keeps.
    expect(screen.getByRole("button", { name: "Copy to my conversations" })).toBeTruthy();
    const note = screen.getByRole("note");
    expect(note.textContent).toContain("only the questions and answers as text");
    expect(note.textContent).toContain("Stopping sharing or deleting the original doesn't take back copies");
    // Only the answer's own copy button is there beside it.
    expect(screen.getAllByRole("button", { name: "Copy answer" })).toHaveLength(1);
  });

  test("a viewer reads and copies as well: the page needs no role", async () => {
    const { server, shared } = setUp();
    openAt(server, sharedConversationPath(shared.id), { identity: signedInBrowser({ role: "viewer" }).browser });
    expect(await screen.findByRole("heading", { level: 1, name: "Boiler room energy" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Copy to my conversations" })).toBeTruthy();
  });

  test("copies into a new conversation of the user's, opens it, and the copy keeps what the text says", async () => {
    const { server, shared } = setUp();
    openAt(server, sharedConversationPath(shared.id));
    click(await screen.findByRole("button", { name: "Copy to my conversations" }));

    // The new conversation's page: the user's own, so with the owner's actions.
    expect(await screen.findByRole("heading", { level: 1, name: "Boiler room energy" })).toBeTruthy();
    await until(() => document.querySelector("[aria-label='Questions and answers']") !== null, "the copy's page");
    expect(await screen.findByRole("button", { name: "Rename" })).toBeTruthy();
    const posts = server.requests.filter((request) => request.method === "POST");
    expect(posts.map((request) => request.url.pathname)).toEqual([`/api/assistant/v1/conversations/${shared.id}/copy`]);
    const copy = [...server.conversations.values()].find((item) => item.id !== shared.id && item.owner !== BOB);
    expect(copy?.title).toBe("Boiler room energy");
    expect(copy?.messages).toEqual(shared.messages);
    expect(copy?.shared_at).toBeUndefined();
  });

  test("a copy opens as the reader's own conversation with the composer, and asking there works", async () => {
    const server = new FakeQuestionServer();
    const shared = server.add({
      title: "Boiler room energy",
      owner: BOB,
      sharedAt: SHARED_AT,
      messages: [
        { kind: "question", text: "How much energy did the boiler room use yesterday?" },
        { kind: "answer", text: "The boiler room used **88 kWh** yesterday." },
      ],
    });
    const rendered = renderAt(sharedConversationPath(shared.id), { conversations: server.api() });
    click(await screen.findByRole("button", { name: "Copy to my conversations" }));
    await until(() => rendered.router.state.location.pathname.startsWith("/conversations/"), "the copy to open");
    const copyId = rendered.router.state.location.pathname.split("/").pop() ?? "";
    expect(copyId).not.toBe(shared.id);
    expect(server.conversations.get(copyId)?.owner).not.toBe(BOB);

    // The copy is the reader's own conversation: the owner's actions and the composer.
    const input = (await screen.findByRole("textbox", { name: "Keep asking TrustIoT" })) as HTMLTextAreaElement;
    expect(screen.getByRole("button", { name: "Rename" })).toBeTruthy();
    act(() => {
      fireEvent.change(input, { target: { value: "And the day before?" } });
    });
    click(screen.getByRole("button", { name: "Send" }));
    await until(() => server.latestRun(copyId)?.status === "running", "the question in the copy to run");
    expect(server.requests.find((request) => request.url.pathname.endsWith("/questions"))?.url.pathname).toBe(
      `/api/assistant/v1/conversations/${copyId}/questions`,
    );
    act(() => server.answer(copyId, "The day before it was **91 kWh**."));
    await until(() => screen.queryByText("91 kWh") !== null, "the answer in the copy");
    // The shared original is untouched: no question went to it and it has no run.
    expect(server.latestRun(shared.id)).toBeUndefined();
    expect(server.conversations.get(shared.id)?.messages).toHaveLength(2);
  }, 90_000);

  test("the shared page has no composer", async () => {
    const server = new FakeQuestionServer();
    const shared = server.add({ title: "Boiler room energy", owner: BOB, sharedAt: SHARED_AT });
    renderAt(sharedConversationPath(shared.id), { conversations: server.api() });
    expect(await screen.findByRole("heading", { level: 1, name: "Boiler room energy" })).toBeTruthy();
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(screen.queryByRole("button", { name: "Send" })).toBeNull();
  });

  test("the copy is listed among the user's conversations", async () => {
    const { server, shared } = setUp();
    const rendered = openAt(server, sharedConversationPath(shared.id));
    click(await screen.findByRole("button", { name: "Copy to my conversations" }));
    await until(() => rendered.router.state.location.pathname.startsWith("/conversations/"), "the copy to open");
    const created = rendered.router.state.location.pathname.split("/").pop() ?? "";
    expect(server.conversations.get(created)?.owner).not.toBe(BOB);
    expect(rendered.router.state.location.pathname).toBe(conversationPath(created));
  });

  test("a refused copy says why and stays on the page, which can try again", async () => {
    const { server, shared } = setUp();
    openAt(server, sharedConversationPath(shared.id));
    server.failNext("copy", fakeError(409, "conflict", "conversation.copy_too_large", { max_bytes: 16_777_216 }));
    click(await screen.findByRole("button", { name: "Copy to my conversations" }));
    expect(await screen.findByText(/Couldn't copy the conversation:/)).toBeTruthy();
    expect(screen.getByRole("heading", { level: 1, name: "Boiler room energy" })).toBeTruthy();
    expect([...server.conversations.values()].filter((item) => item.owner !== BOB)).toHaveLength(0);
    click(screen.getByRole("button", { name: "Copy to my conversations" }));
    await until(
      () => [...server.conversations.values()].some((item) => item.owner !== BOB),
      "the second try to make the copy",
    );
  });

  test("the button cannot send twice while a copy runs", async () => {
    const { server, shared } = setUp();
    openAt(server, sharedConversationPath(shared.id));
    const button = await screen.findByRole("button", { name: "Copy to my conversations" });
    server.hold(["copy"]);
    click(button);
    const busy = await screen.findByRole("button", { name: "Copying…" });
    expect((busy as HTMLButtonElement).disabled).toBe(true);
    click(busy);
    server.release();
    await until(() => [...server.conversations.values()].some((item) => item.owner !== BOB), "the copy");
    expect(server.requests.filter((request) => request.method === "POST")).toHaveLength(1);
  });

  test("a conversation that is not shared, is the user's own, is deleted or does not exist is the same page", async () => {
    const { server, shared, private_ } = setUp();
    const own = server.add({ title: "Mine", sharedAt: SHARED_AT });
    const deleted = server.add({ title: "Gone", owner: BOB, deletedAt: "2026-11-03T00:00:00Z" });
    for (const id of [private_.id, own.id, deleted.id, fakeUuid()]) {
      const { unmount } = openAt(server, sharedConversationPath(id));
      expect(await screen.findByRole("heading", { name: "Shared conversation not found" })).toBeTruthy();
      expect(screen.getByRole("link", { name: "Go to shared conversations" }).getAttribute("href")).toBe(
        sharedConversationsPath(),
      );
      unmount();
    }
    expect(shared.owner).toBe(BOB);
  });

  test("an ID that is not a UUID asks the server nothing", async () => {
    const { server } = setUp();
    openAt(server, "/shared-conversations/not-an-id");
    expect(await screen.findByRole("heading", { name: "Shared conversation not found" })).toBeTruthy();
    expect(server.requests.filter((request) => request.url.pathname.includes("shared-conversations/"))).toEqual([]);
  });

  test("a failure to load shows a retry, not the not-found page", async () => {
    const { server, shared } = setUp();
    server.failNext("getShared", fakeError(503, "unavailable", "error.unavailable"));
    openAt(server, sharedConversationPath(shared.id));
    expect(await screen.findByText(/Couldn't load this shared conversation:/)).toBeTruthy();
    click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Boiler room energy" })).toBeTruthy();
  });

  test("shows a skeleton while it loads", async () => {
    const { server, shared } = setUp();
    server.hold(["getShared"]);
    openAt(server, sharedConversationPath(shared.id));
    expect(await screen.findByText("Loading the shared conversation…")).toBeTruthy();
    server.release();
    expect(await screen.findByRole("heading", { level: 1, name: "Boiler room energy" })).toBeTruthy();
  });

  test("a link in an answer does not lead back to a conversation the reader cannot open", async () => {
    const server = new FakeConversationServer();
    const shared = server.add({
      title: "Links",
      owner: BOB,
      sharedAt: SHARED_AT,
      messages: [
        { kind: "question", text: "Where?" },
        { kind: "answer", text: "See [the dashboards](/dashboards)." },
      ],
    });
    openAt(server, sharedConversationPath(shared.id));
    const link = await screen.findByRole("link", { name: "the dashboards" });
    click(link);
    // The dashboards page's way back is not the shared conversation (it would be the owner's path).
    expect(await screen.findByRole("heading", { name: "Saved dashboards" })).toBeTruthy();
    const back = screen.queryByRole("link", { name: "Back to the conversation" });
    expect(back?.getAttribute("href") ?? "/").not.toContain(shared.id);
  });
});

describe("sharing from one member to another, through the service's rules", () => {
  test("A shares, B finds it, reads it, copies it; A stops sharing, it leaves B's list and the copy stays", async () => {
    const server = new FakeConversationServer();
    const mine = server.add({ title: "Energy this week" });
    const bob = server.api(BOB);

    // B sees nothing before A shares, and the read-only page and the copy are 404.
    expect((await bob.listShared({})).conversations).toEqual([]);
    await expect(bob.getShared(mine.id)).rejects.toMatchObject({ status: 404 });
    await expect(bob.copyConversation(mine.id)).rejects.toMatchObject({ status: 404 });

    await server.api().shareConversation(mine.id);
    const list = await bob.listShared({});
    expect(list.conversations.map((item) => item.id)).toEqual([mine.id]);
    const detail = await bob.getShared(mine.id);
    expect(detail.messages).toEqual(mine.messages);
    // B cannot touch what A owns.
    await expect(bob.shareConversation(mine.id)).rejects.toMatchObject({ status: 404 });
    await expect(bob.updateConversation(mine.id, { title: "x" })).rejects.toMatchObject({ status: 404 });
    await expect(bob.deleteConversation(mine.id)).rejects.toMatchObject({ status: 404 });

    const copy = await bob.copyConversation(mine.id);
    expect(server.conversations.get(copy.id)?.owner).toBe(BOB);

    await server.api().unshareConversation(mine.id);
    expect((await bob.listShared({})).conversations).toEqual([]);
    await expect(bob.getShared(mine.id)).rejects.toMatchObject({ status: 404 });
    expect((await bob.getConversation(copy.id)).messages).toEqual(mine.messages);

    // Deleting a shared conversation unshares it, and restoring does not share it again.
    await server.api().shareConversation(mine.id);
    await server.api().deleteConversation(mine.id);
    expect((await bob.listShared({})).conversations).toEqual([]);
    await server.api().restoreConversation(mine.id);
    expect((await bob.listShared({})).conversations).toEqual([]);
    expect(server.conversations.get(mine.id)?.shared_at).toBeUndefined();
  });
});

describe("a shared conversation's dashboard drafts", () => {
  const format = dashboardFormat("en", "UTC");

  /** Bob's shared conversation with a draft; the reader's role is `role`. */
  function withDraft(role: FakeRole) {
    const { server, shared } = setUp();
    const dashboards = new FakeDashboardServer();
    dashboards.role = role;
    const draft = dashboards.addDashboard({
      kind: "draft",
      conversationId: shared.id,
      title: "Boiler room energy",
      versions: [[SPECS.kpi]],
    });
    const chart = fakeEngine();
    const rendered = openAt(server, sharedConversationPath(shared.id), {
      dashboards: dashboards.api(),
      chartEngine: chart.engine,
      identity: signedInBrowser({ role }).browser,
    });
    return { server, shared, dashboards, draft, ...rendered };
  }

  for (const role of ["viewer", "editor", "admin"] as const) {
    test(
      `a ${role} sees the draft beside the conversation, drawn by the same components, with nothing to save or change`,
      async () => {
        const { dashboards } = withDraft(role);
        expect(await screen.findByRole("heading", { level: 2, name: "Boiler room energy" })).toBeTruthy();
        expect(screen.getByText("Draft · version 1 · not saved")).toBeTruthy();
        const preview = screen.getByRole("region", { name: "Draft preview" });
        const value = await within(
          await within(preview).findByRole("region", { name: "Energy this week" }),
        ).findByTestId("value");
        // The number is the dataset's, queried for the reader.
        const total = DAILY_ROWS.reduce((sum, row) => sum + (typeof row[2] === "number" ? row[2] : 0), 0);
        await until(() => value.textContent === format.number(total), "the draft's number to be drawn");
        // Read only, whatever the role: no save, no edit, no word about who sees it, one note saying why.
        expect(screen.queryByRole("button", { name: "Save to dashboard list" })).toBeNull();
        expect(screen.queryByText(/Only you can see it, here/)).toBeNull();
        expect(screen.getByText(/This draft is in a conversation another member shared/)).toBeTruthy();
        expect(screen.queryByRole("button", { name: /^Save|^Publish|^Edit|^Discard|^Restore/ })).toBeNull();
        // Looking changes nothing: only reads were sent for the draft.
        expect(
          dashboards.requests.every(
            (request) => request.method === "GET" || request.url.pathname.endsWith("/dataset-runs"),
          ),
        ).toBe(true);
        expect(dashboards.requests.some((request) => request.url.pathname.endsWith("/publish"))).toBe(false);
      },
      LONG,
    );
  }

  test(
    "the reader's page offers nothing of the owner's answers (no saving from an answer) and calls no owner-only endpoint",
    async () => {
      const server = new FakeQuestionServer();
      const dashboards = new FakeDashboardServer();
      dashboards.role = "editor";
      const shared = server.add({
        title: "Boiler room energy",
        owner: BOB,
        sharedAt: SHARED_AT,
      });
      const draft = dashboards.addDashboard({
        kind: "draft",
        conversationId: shared.id,
        title: "Boiler room energy",
        versions: [[SPECS.kpi]],
      });
      shared.messages = [
        { kind: "question", text: "Show me the boiler room." },
        {
          kind: "answer",
          text: "Here is the dashboard.",
          draft_versions: [{ dashboard_id: draft.id, version: 1 }],
        },
      ];
      renderAt(sharedConversationPath(shared.id), {
        conversations: server.api(),
        dashboards: dashboards.api(),
        chartEngine: fakeEngine().engine,
        identity: signedInBrowser({ role: "editor" }).browser,
      });
      const article = await screen.findByRole("article");
      expect(within(article).getByText("Here is the dashboard.")).toBeTruthy();
      // The panel beside the conversation draws the draft; the answer does not, and has no way to save it.
      expect(await screen.findByRole("heading", { level: 2, name: "Boiler room energy" })).toBeTruthy();
      expect(within(article).queryByRole("region")).toBeNull();
      expect(within(article).queryByRole("button", { name: /Save|Pin/ })).toBeNull();
      expect(screen.queryByRole("button", { name: /Save|Pin|Stop|Regenerate|Resume|Send/ })).toBeNull();
      expect(screen.queryByRole("textbox")).toBeNull();
      // Nor the context ask-AI buttons (Laysi/trustiot-v2#158): they would ask in the owner's
      // conversation, so the read-only page has no "Ask AI" row and no question button anywhere.
      expect(screen.queryByRole("group", { name: /^Ask AI/ })).toBeNull();
      expect(
        screen.queryByRole("button", {
          name: /Why this number\?|What stands out\?|Compare with last week|Analyze this stretch/,
        }),
      ).toBeNull();
      // Only the shared endpoint was read on the conversation side (and the shell's own list of the
      // reader's conversations): nothing of the owner's conversation, its questions, runs or answer
      // stream, and nothing that changes it.
      const calls = server.requests.map((request) => `${request.method} ${request.url.pathname}`);
      expect(calls.filter((call) => call !== "GET /api/assistant/v1/conversations")).toEqual([
        `GET /api/assistant/v1/shared-conversations/${shared.id}`,
      ]);
      expect(server.streamOpens).toEqual([]);
      expect(dashboards.requests.some((request) => request.url.pathname.endsWith("/publish"))).toBe(false);
    },
    LONG,
  );

  test(
    "a conversation without drafts has no panel, and a draft list that cannot be read says so and can be retried",
    async () => {
      const { server, shared } = setUp();
      const dashboards = new FakeDashboardServer();
      dashboards.failNext("drafts", fakeError(503, "unavailable", "error.unavailable"));
      openAt(server, sharedConversationPath(shared.id), { dashboards: dashboards.api() });
      expect(await screen.findByText(/This conversation's drafts couldn't be loaded/)).toBeTruthy();
      // The conversation itself is there.
      expect(screen.getByRole("heading", { level: 1, name: "Boiler room energy" })).toBeTruthy();
      click(screen.getAllByRole("button", { name: "Try again" })[0] as HTMLElement);
      await until(() => screen.queryByText(/This conversation's drafts couldn't be loaded/) === null, "the retry");
      expect(screen.queryByRole("region", { name: "Draft preview" })).toBeNull();
    },
    LONG,
  );
});
