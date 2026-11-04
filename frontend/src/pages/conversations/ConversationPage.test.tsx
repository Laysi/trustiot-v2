import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath } from "../../app/paths.ts";
import { markdownFileName } from "../../chat/export.ts";
import { formatDateTime } from "../../i18n/format.ts";
import { FakeConversationServer } from "../../test/fake-conversations.ts";
import { fakeError } from "../../test/fake-dashboards.ts";
import { renderAt } from "../../test/render.tsx";

const QUESTION = "How did floor 3 use energy this week?\nAnd last week?";
const ANSWER = [
  "Floor 3 used **412 kWh** this week.",
  "",
  "| Week | kWh |",
  "| - | -: |",
  "| This | 412 |",
  "",
  `![plan](https://evil.example.test/p.png) <img src=x> [report](https://evil.example.test/r)`,
].join("\n");

let clipboard: string[] = [];
let failClipboard = false;
let downloads: { name: string; blob: Blob }[] = [];
const restore: (() => void)[] = [];

beforeEach(() => {
  clipboard = [];
  failClipboard = false;
  downloads = [];
  Object.defineProperty(navigator, "clipboard", {
    configurable: true,
    value: {
      writeText: async (text: string) => {
        if (failClipboard) throw new DOMException("denied", "NotAllowedError");
        clipboard.push(text);
      },
    },
  });
  const blobs = new Map<string, Blob>();
  const createObjectURL = URL.createObjectURL;
  const revokeObjectURL = URL.revokeObjectURL;
  const click = HTMLAnchorElement.prototype.click;
  URL.createObjectURL = (blob: Blob) => {
    const url = `blob:https://app.example.test/${blobs.size}`;
    blobs.set(url, blob);
    return url;
  };
  URL.revokeObjectURL = () => {};
  HTMLAnchorElement.prototype.click = function (this: HTMLAnchorElement) {
    const blob = blobs.get(this.href);
    if (blob && this.download) downloads.push({ name: this.download, blob });
  };
  restore.push(() => {
    URL.createObjectURL = createObjectURL;
    URL.revokeObjectURL = revokeObjectURL;
    HTMLAnchorElement.prototype.click = click;
  });
});

afterEach(() => {
  for (const undo of restore.splice(0)) undo();
  cleanup();
});

const when = (iso: string) => formatDateTime(iso, "en", "UTC");

function setUp(options: Parameters<FakeConversationServer["add"]>[0] = {}) {
  const server = new FakeConversationServer();
  const conversation = server.add({
    title: "Energy this week",
    createdAt: "2026-11-01T01:00:00Z",
    activityAt: "2026-11-02T03:00:00Z",
    messages: [
      { kind: "question", text: QUESTION },
      { kind: "answer", text: ANSWER },
      { kind: "question", text: "Thanks" },
      { kind: "answer", text: "You're welcome." },
    ],
    ...options,
  });
  const rendered = renderAt(conversationPath(conversation.id), { conversations: server.api() });
  return { server, conversation, ...rendered };
}

async function openActions() {
  const trigger = await screen.findByRole("button", { name: "Conversation actions" });
  await act(async () => {
    fireEvent.keyDown(trigger, { key: "Enter" });
  });
  return screen.findByRole("menu");
}

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

describe("a conversation's page", () => {
  test("shows its questions and answers in order, in one column, with its title and times", async () => {
    setUp();
    expect(await screen.findByRole("heading", { level: 1, name: "Energy this week" })).toBeTruthy();
    expect(
      screen.getByText(`Started ${when("2026-11-01T01:00:00Z")} · Last activity ${when("2026-11-02T03:00:00Z")}`),
    ).toBeTruthy();
    const thread = screen.getByRole("list", { name: "Questions and answers" });
    const items = within(thread)
      .getAllByRole("listitem")
      .filter((item) => item.parentElement === thread);
    expect(items.map((item) => (within(item).queryByRole("article") ? "answer" : "question"))).toEqual([
      "question",
      "answer",
      "question",
      "answer",
    ]);
    // A question is the text as typed, line breaks kept; it is never Markdown.
    expect(items[0]?.textContent).toBe(`You asked:${QUESTION}`);
    const answers = screen.getAllByRole("article", { name: "TrustIoT's answer" });
    expect(answers[0]?.querySelector("strong")?.textContent).toBe("412 kWh");
    expect(answers[0]?.querySelector("table")).toBeTruthy();
    // The answer's rules hold on the page (safe-markdown.test.tsx has every case).
    expect(answers[0]?.querySelector("img")).toBeNull();
    expect(answers[0]?.querySelector("a")).toBeNull();
    expect(answers[0]?.textContent).toContain("report (https://evil.example.test/r)");
    // No top-bar way back: a conversation is not a secondary page.
    expect(screen.queryByRole("link", { name: "Back to the conversation" })).toBeNull();
    expect(screen.queryByRole("textbox")).toBeNull();
  });

  test("each answer copies its text exactly as the API gave it, and says so", async () => {
    setUp();
    const answers = await screen.findAllByRole("article", { name: "TrustIoT's answer" });
    const first = answers[0] as HTMLElement;
    await act(async () => {
      fireEvent.click(within(first).getByRole("button", { name: "Copy answer" }));
    });
    expect(clipboard).toEqual([ANSWER]);
    expect(within(first).getByRole("status").textContent).toBe("Answer copied");

    failClipboard = true;
    const second = answers[1] as HTMLElement;
    await act(async () => {
      fireEvent.click(within(second).getByRole("button", { name: "Copy answer" }));
    });
    expect(within(second).getByRole("status").textContent).toContain("Couldn't copy");
  });

  test("exports the whole conversation as Markdown: the title, then the API's texts, under a safe name", async () => {
    setUp({ title: 'Energy: "floor 3" / week?' });
    const menu = await openActions();
    click(within(menu).getByRole("menuitem", { name: "Export as Markdown" }));
    expect(downloads).toHaveLength(1);
    const [download] = downloads;
    expect(download?.name).toBe("Energy floor 3 week.md");
    expect(download?.blob.type).toBe("text/markdown;charset=utf-8");
    expect(await download?.blob.text()).toBe(
      [
        '# Energy: "floor 3" / week?',
        "## Question",
        QUESTION,
        "## Answer",
        ANSWER,
        "## Question",
        "Thanks",
        "## Answer",
        "You're welcome.",
      ]
        .join("\n\n")
        .concat("\n"),
    );
    expect(await screen.findByText("Exported a Markdown file.")).toBeTruthy();
  });

  test("renames in place, and the new title is the heading", async () => {
    const { server, conversation } = setUp();
    click(await screen.findByRole("button", { name: "Rename" }));
    const field = screen.getByRole("textbox", { name: "Conversation title" });
    act(() => {
      fireEvent.change(field, { target: { value: "Floor 3" } });
    });
    act(() => {
      fireEvent.submit(field);
    });
    expect(await screen.findByRole("heading", { level: 1, name: "Floor 3" })).toBeTruthy();
    expect(screen.getByText("Renamed.")).toBeTruthy();
    expect(server.conversations.get(conversation.id)?.title).toBe("Floor 3");
  });

  test("archives from its menu, then carries the archived tag, and unarchives", async () => {
    const { server, conversation } = setUp();
    await screen.findByRole("heading", { level: 1, name: "Energy this week" });
    expect(screen.queryByText("Archived")).toBeNull();
    click(within(await openActions()).getByRole("menuitem", { name: "Archive" }));
    expect(await screen.findByText(/This conversation is archived/)).toBeTruthy();
    await waitFor(() => expect(screen.getByText("Archived")).toBeTruthy());
    expect(server.conversations.get(conversation.id)?.archived_at).toBeDefined();

    click(within(await openActions()).getByRole("menuitem", { name: "Unarchive" }));
    expect(await screen.findByText(/Unarchived\. The conversation is back in use\./)).toBeTruthy();
    await waitFor(() => expect(screen.queryByText("Archived")).toBeNull());
  });

  test("deleting returns to the history, which says it can be restored under Deleted", async () => {
    const { router, server, conversation } = setUp();
    click(within(await openActions()).getByRole("menuitem", { name: "Delete" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete “Energy this week”?" });
    click(within(dialog).getByRole("button", { name: "Delete conversation" }));
    expect(
      await screen.findByText("Deleted “Energy this week”. You can restore it under “Deleted” for 30 days."),
    ).toBeTruthy();
    expect(router.state.location.pathname).toBe("/conversations");
    expect(server.conversations.get(conversation.id)?.deleted_at).toBeDefined();
    // The notice does not come back on a reload or going back.
    await waitFor(() => expect(router.state.location.state).toBeNull());
    click(screen.getByRole("button", { name: "Show deleted conversations" }));
    expect(await screen.findByRole("table")).toBeTruthy();
    expect(router.state.location.search).toBe("?tab=deleted");
  });

  test("an unknown, deleted or malformed conversation is not found, with the way to the history", async () => {
    const server = new FakeConversationServer();
    const deleted = server.add({ deletedAt: "2026-11-01T00:00:00Z" });
    for (const path of [conversationPath(deleted.id), "/conversations/not-a-uuid"]) {
      const { unmount } = renderAt(path, { conversations: server.api() });
      expect(await screen.findByRole("heading", { name: "Conversation not found" })).toBeTruthy();
      expect(screen.getByRole("link", { name: "Go to conversations" }).getAttribute("href")).toBe("/conversations");
      unmount();
      cleanup();
    }
    // A malformed ID is never sent.
    expect(server.requests.filter((request) => request.url.pathname.includes("not-a-uuid"))).toEqual([]);
  });

  test("a failure to load says why and retries", async () => {
    const server = new FakeConversationServer();
    const conversation = server.add({ title: "Energy this week" });
    server.failNext("get", fakeError(503, "unavailable", "error.unavailable"));
    renderAt(conversationPath(conversation.id), { conversations: server.api() });
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Couldn't load this conversation");
    click(within(alert).getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Energy this week" })).toBeTruthy();
  });

  test("a conversation with no messages says so", async () => {
    setUp({ messages: [] });
    expect(await screen.findByText("This conversation has no questions or answers yet.")).toBeTruthy();
  });
});

describe("the export's file name", () => {
  test.each([
    ["Energy this week", "Energy this week.md"],
    ["../../etc/passwd", "etc passwd.md"],
    ["a\u202egnp.exe", "a gnp.exe.md"],
    ["CON", "TrustIoT conversation.md"],
    ["con.txt", "TrustIoT conversation.md"],
    ["lpt1.report", "TrustIoT conversation.md"],
    ["COM¹", "TrustIoT conversation.md"],
    ["Console report", "Console report.md"],
    ["   ...   ", "TrustIoT conversation.md"],
    ["x".repeat(300), `${"x".repeat(80)}.md`],
    ["Tab\there\nnew line", "Tab here new line.md"],
  ])("%j is saved as %j", (title, name) => {
    expect(markdownFileName(title, "TrustIoT conversation")).toBe(name);
  });
});
