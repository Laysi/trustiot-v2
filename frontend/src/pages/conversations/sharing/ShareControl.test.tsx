import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, within } from "@testing-library/react";

import { conversationPath } from "../../../app/paths.ts";
import { formatDateTime } from "../../../i18n/format.ts";
import { FakeConversationServer } from "../../../test/fake-conversations.ts";
import { fakeError } from "../../../test/fake-dashboards.ts";
import { renderAt } from "../../../test/render.tsx";
import { LONG, until } from "../../../test/until.ts";

afterEach(() => {
  cleanup();
});

function setUp(options: Parameters<FakeConversationServer["add"]>[0] = {}) {
  const server = new FakeConversationServer();
  const conversation = server.add({ title: "Energy this week", ...options });
  renderAt(conversationPath(conversation.id), { conversations: server.api() });
  return { server, conversation };
}

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

const when = (iso: string) => formatDateTime(iso, "en", "UTC");

// The page is big and a role query over all of it is slow (and, run late in the whole suite, was
// seen to take the test runner down), so the waits here look at the document by selector.
const dialogs = () => document.querySelectorAll("[role=alertdialog]");

async function openDialog(): Promise<HTMLElement> {
  await until(() => dialogs().length === 1, "the confirmation dialog");
  return dialogs()[0] as HTMLElement;
}

async function pageButton(name: string): Promise<HTMLElement> {
  const find = () =>
    [...document.querySelectorAll("button")].find((button) => button.textContent?.trim() === name) as
      | HTMLElement
      | undefined;
  await until(() => find() !== undefined, `the "${name}" button`);
  return find() as HTMLElement;
}

describe("the share switch on the conversation page", () => {
  test("shares with the whole tenant after a confirmation that names who sees it and what stays", async () => {
    const { server, conversation } = setUp();
    click(await pageButton("Share"));

    const dialog = await openDialog();
    expect(within(dialog).getByText("Share with everyone in Acme Factory?")).toBeTruthy();
    expect(within(dialog).getByText(/Every member of Acme Factory will find this conversation/)).toBeTruthy();
    // The answers were made with the owner's permissions, and copies are not taken back.
    const note = within(dialog).getByRole("note");
    expect(note.textContent).toContain("may contain data other members can't normally see");
    expect(note.textContent).toContain(
      "Stopping sharing or deleting the conversation doesn't take back copies that members already made.",
    );
    // Nothing happened yet.
    expect(server.conversations.get(conversation.id)?.shared_at).toBeUndefined();

    click(within(dialog).getByRole("button", { name: "Share with the organization" }));
    await until(
      () => server.conversations.get(conversation.id)?.shared_at !== undefined,
      "the conversation to be shared",
    );
    expect(await screen.findByText(/Shared with every member of Acme Factory\./)).toBeTruthy();
    expect(dialogs()).toHaveLength(0);
    // The conversation is read again: the button now says it is shared.
    expect(await pageButton("Shared")).toBeTruthy();
  });

  test("cancelling the confirmation shares nothing", async () => {
    const { server, conversation } = setUp();
    click(await pageButton("Share"));
    click(within(await openDialog()).getByRole("button", { name: "Cancel" }));
    await until(() => dialogs().length === 0, "the dialog to close");
    expect(server.requests.some((request) => request.method === "PUT")).toBe(false);
    expect(server.conversations.get(conversation.id)?.shared_at).toBeUndefined();
  });

  test("a shared conversation says so, and stopping sharing says that copies stay", async () => {
    const { server, conversation } = setUp({ sharedAt: "2026-11-02T02:00:00Z" });
    click(await pageButton("Shared"));

    const dialog = await openDialog();
    expect(within(dialog).getByText("Stop sharing this conversation?")).toBeTruthy();
    expect(within(dialog).getByText(`since ${when("2026-11-02T02:00:00Z")}`, { exact: false })).toBeTruthy();
    const note = within(dialog).getByRole("note");
    expect(note.textContent).toContain("Stopping sharing doesn't take back copies that members already made");
    expect(note.textContent).toContain("Deleting this conversation doesn't take them back either");

    click(within(dialog).getByRole("button", { name: "Stop sharing" }));
    await until(() => server.conversations.get(conversation.id)?.shared_at === undefined, "sharing to stop");
    expect(await screen.findByText("Stopped sharing. Copies that members already made stay.")).toBeTruthy();
    expect(await pageButton("Share")).toBeTruthy();
  });

  test("an error stays in the dialog, which stays open, and nothing changes", async () => {
    const { server, conversation } = setUp();
    click(await pageButton("Share"));
    server.failNext("share", fakeError(503, "unavailable", "error.unavailable"));
    click(within(await openDialog()).getByRole("button", { name: "Share with the organization" }));
    expect(await screen.findByText(/Couldn't change sharing:/)).toBeTruthy();
    expect(dialogs()).toHaveLength(1);
    expect(server.conversations.get(conversation.id)?.shared_at).toBeUndefined();
    // Try again: it works, the dialog closes.
    click(within(await openDialog()).getByRole("button", { name: "Share with the organization" }));
    await until(() => dialogs().length === 0, "the dialog to close after the retry");
    expect(server.conversations.get(conversation.id)?.shared_at).toBeDefined();
  });

  test(
    "while the request runs the dialog cannot be left or sent twice",
    async () => {
      const { server } = setUp();
      click(await pageButton("Share"));
      server.hold(["share"]);
      const dialog = await openDialog();
      click(within(dialog).getByRole("button", { name: "Share with the organization" }));
      const busy = await within(dialog).findByRole("button", { name: "Saving…" });
      expect((busy as HTMLButtonElement).disabled).toBe(true);
      expect((within(dialog).getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(true);
      server.release();
      await until(
        () => document.querySelector("[role=alertdialog]") === null,
        "the dialog to close after the held request",
      );
      expect(server.requests.filter((request) => request.method === "PUT")).toHaveLength(1);
    },
    LONG + 10_000,
  );

  test("sharing a conversation that was deleted meanwhile is refused, and the page then says it is gone", async () => {
    const { server, conversation } = setUp();
    click(await pageButton("Share"));
    const stored = server.conversations.get(conversation.id);
    if (stored === undefined) throw new Error("the conversation is missing");
    stored.deleted_at = "2026-11-03T00:00:00Z";
    click(within(await openDialog()).getByRole("button", { name: "Share with the organization" }));
    // The refusal is a 404, and reading the conversation again gives the page's "not found".
    expect(await screen.findByRole("heading", { name: "Conversation not found" })).toBeTruthy();
    expect(stored.shared_at).toBeUndefined();
  });
});
