import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath } from "../../app/paths.ts";
import { formatDateTime } from "../../i18n/format.ts";
import { FakeConversationServer } from "../../test/fake-conversations.ts";
import { fakeError } from "../../test/fake-dashboards.ts";
import { renderAt } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

const when = (iso: string) => formatDateTime(iso, "en", "UTC");

/** `count` conversations "Talk 1" (the most recent) to "Talk <count>". */
function many(server: FakeConversationServer, count: number) {
  const start = Date.parse("2026-10-01T00:00:00Z");
  for (let index = count; index >= 1; index -= 1) {
    server.add({ title: `Talk ${index}`, activityAt: new Date(start + (count - index) * 60_000).toISOString() });
  }
}

/**
 * The history's own list requests: the top bar also reads the latest conversations (five), for
 * its way back to the conversation (app/back-to-conversation.ts).
 */
function historyRequests(server: FakeConversationServer): string[] {
  return server.listRequests().filter((request) => !request.endsWith("?limit=5"));
}

function open(server: FakeConversationServer, path = "/conversations") {
  return renderAt(path, { conversations: server.api() });
}

/** The titles in the table, in order. */
function listedTitles(): string[] {
  const table = screen.getByRole("table");
  return within(table)
    .getAllByRole("rowheader")
    .map((cell) => (within(cell).queryByRole("link")?.textContent ?? cell.textContent ?? "").trim());
}

function rowOf(title: string): HTMLElement {
  const cell = within(screen.getByRole("table"))
    .getAllByRole("rowheader")
    .find((header) => header.textContent?.includes(title));
  const row = cell?.closest("tr");
  if (!(row instanceof HTMLElement)) throw new Error(`no row for ${title}`);
  return row;
}

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

describe("the conversation history", () => {
  test("lists the user's conversations, the most recent first, each linking to its page", async () => {
    const server = new FakeConversationServer();
    const older = server.add({ title: "Chiller load", activityAt: "2026-10-30T08:00:00Z" });
    const newer = server.add({ title: "Energy this week", activityAt: "2026-11-01T08:00:00Z" });
    open(server);
    await screen.findByRole("table");
    expect(screen.getByRole("heading", { level: 1, name: "Conversations" })).toBeTruthy();
    expect(listedTitles()).toEqual(["Energy this week", "Chiller load"]);
    const links = within(screen.getByRole("table")).getAllByRole("link");
    expect(links.map((link) => link.getAttribute("href"))).toEqual([
      conversationPath(newer.id),
      conversationPath(older.id),
    ]);
    expect(within(rowOf("Energy this week")).getByText(when("2026-11-01T08:00:00Z"))).toBeTruthy();
    // One page of the service's most, with nothing else asked for.
    expect(historyRequests(server)).toEqual(["/api/assistant/v1/conversations?limit=100"]);
  });

  test("a conversation without a title is shown by when it started", async () => {
    const server = new FakeConversationServer();
    server.add({ title: null, createdAt: "2026-11-02T09:30:00Z" });
    open(server);
    await screen.findByRole("table");
    expect(listedTitles()).toEqual([`Conversation started ${when("2026-11-02T09:30:00Z")}`]);
  });

  test("the three tabs read their own lists, the choice is in the address, and a link opens a tab", async () => {
    const server = new FakeConversationServer();
    server.add({ title: "Current" });
    server.add({ title: "Old plant", archivedAt: "2026-10-31T00:00:00Z" });
    server.add({ title: "Gone", deletedAt: "2026-11-01T00:00:00Z" });
    const { router } = open(server);
    await screen.findByRole("table");
    expect(listedTitles()).toEqual(["Current"]);
    expect(screen.getByRole("button", { name: "In use" }).getAttribute("aria-pressed")).toBe("true");

    click(screen.getByRole("button", { name: "Archived" }));
    await waitFor(() => expect(listedTitles()).toEqual(["Old plant"]));
    expect(router.state.location.search).toBe("?tab=archived");
    expect(server.listRequests().at(-1)).toBe("/api/assistant/v1/conversations?archived=true&limit=100");
    expect(screen.getByText(`Archived ${when("2026-10-31T00:00:00Z")}`)).toBeTruthy();

    click(screen.getByRole("button", { name: "Deleted" }));
    await waitFor(() => expect(listedTitles()).toEqual(["Gone"]));
    expect(router.state.location.search).toBe("?tab=deleted");
    expect(server.requests.at(-1)?.url.pathname).toBe("/api/assistant/v1/deleted-conversations");
    expect(screen.getByRole("note").textContent).toContain("deleted permanently");
    // When it was deleted and until when it can be restored; deleted conversations have no page.
    const row = rowOf("Gone");
    expect(row.textContent).toContain(when("2026-11-01T00:00:00Z"));
    expect(row.textContent).toContain(when("2026-12-01T00:00:00.000Z"));
    expect(within(row).queryByRole("link")).toBeNull();

    click(screen.getByRole("button", { name: "In use" }));
    await waitFor(() => expect(listedTitles()).toEqual(["Current"]));
    expect(router.state.location.search).toBe("");

    cleanup();
    open(server, "/conversations?tab=deleted");
    await screen.findByRole("table");
    expect(listedTitles()).toEqual(["Gone"]);
    expect(screen.getByRole("button", { name: "Deleted" }).getAttribute("aria-pressed")).toBe("true");
  });

  test("the pages are read one after another to the last", async () => {
    const server = new FakeConversationServer();
    many(server, 205);
    open(server);
    await screen.findByRole("table");
    expect(listedTitles()).toHaveLength(100);
    expect(screen.getByText("100 shown so far · there are more")).toBeTruthy();
    click(screen.getByRole("button", { name: "Show more" }));
    await waitFor(() => expect(listedTitles()).toHaveLength(200));
    click(screen.getByRole("button", { name: "Show more" }));
    await waitFor(() => expect(listedTitles()).toHaveLength(205));
    expect(listedTitles().at(-1)).toBe("Talk 205");
    expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
    expect(screen.getByText("205 shown · by last activity")).toBeTruthy();
    expect(historyRequests(server)).toEqual([
      "/api/assistant/v1/conversations?limit=100",
      "/api/assistant/v1/conversations?cursor=100&limit=100",
      "/api/assistant/v1/conversations?cursor=200&limit=100",
    ]);
  });

  test("the search matches titles only, and reads the pages not loaded yet", async () => {
    const server = new FakeConversationServer();
    many(server, 150);
    // Only the second page has it, and its answer has the word but does not count.
    server.add({ title: "Boiler pressure", activityAt: "2026-09-01T00:00:00Z" });
    server.add({
      title: "Unrelated",
      activityAt: "2026-11-01T00:00:00Z",
      messages: [{ kind: "answer", text: "boiler" }],
    });
    open(server);
    await screen.findByRole("table");
    expect(listedTitles()).not.toContain("Boiler pressure");
    act(() => {
      fireEvent.change(screen.getByRole("searchbox", { name: "Search conversations" }), {
        target: { value: "  BOILER " },
      });
    });
    await waitFor(() => expect(listedTitles()).toEqual(["Boiler pressure"]));
    expect(historyRequests(server)).toEqual([
      "/api/assistant/v1/conversations?limit=100",
      "/api/assistant/v1/conversations?cursor=100&limit=100",
    ]);
    expect(screen.getByText("1 shown · by last activity")).toBeTruthy();

    act(() => {
      fireEvent.change(screen.getByRole("searchbox", { name: "Search conversations" }), {
        target: { value: "furnace" },
      });
    });
    expect(await screen.findByText("No conversation title matches “furnace”")).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
  });

  test("with nothing in a tab it says what goes there", async () => {
    open(new FakeConversationServer());
    expect(await screen.findByText("No conversations yet")).toBeTruthy();
    click(screen.getByRole("button", { name: "Archived" }));
    expect(await screen.findByText("No archived conversations")).toBeTruthy();
    click(screen.getByRole("button", { name: "Deleted" }));
    expect(await screen.findByText("No deleted conversations")).toBeTruthy();
  });

  test("while it loads one status says so; a failure says why and retries", async () => {
    const server = new FakeConversationServer();
    server.add({ title: "Energy this week" });
    // The history's list and the top bar's latest conversations both fail.
    server.failNext("list", fakeError(503, "unavailable", "error.unavailable"));
    server.failNext("list", fakeError(503, "unavailable", "error.unavailable"));
    server.hold(["list"]);
    open(server);
    expect(await screen.findByText("Loading conversations…")).toBeTruthy();
    act(() => server.release());
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Couldn't load the conversations");
    click(within(alert).getByRole("button", { name: "Try again" }));
    await screen.findByRole("table");
    expect(listedTitles()).toEqual(["Energy this week"]);
  });
});

describe("changing a conversation from the history", () => {
  test("renames in place: Enter saves, Escape leaves, and a refused title is said at the field", async () => {
    const server = new FakeConversationServer();
    const conversation = server.add({ title: "Energy this week" });
    open(server);
    await screen.findByRole("table");
    click(within(rowOf("Energy this week")).getByRole("button", { name: "Rename “Energy this week”" }));
    const field = screen.getByRole("textbox", { name: "Conversation title" }) as HTMLInputElement;
    expect(field.value).toBe("Energy this week");
    expect(document.activeElement).toBe(field);

    act(() => {
      fireEvent.change(field, { target: { value: "bad\u202etitle" } });
    });
    click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(field.getAttribute("aria-invalid")).toBe("true"));
    expect(field.value).toBe("bad\u202etitle");
    expect(screen.getByText(/Enter a title of 1 to 200 characters/)).toBeTruthy();

    act(() => {
      fireEvent.keyDown(field, { key: "Escape" });
    });
    expect(screen.queryByRole("textbox", { name: "Conversation title" })).toBeNull();
    expect(listedTitles()).toEqual(["Energy this week"]);

    click(within(rowOf("Energy this week")).getByRole("button", { name: "Rename “Energy this week”" }));
    const again = screen.getByRole("textbox", { name: "Conversation title" });
    act(() => {
      fireEvent.change(again, { target: { value: "  Floor 3 energy  " } });
    });
    act(() => {
      fireEvent.submit(again);
    });
    await waitFor(() => expect(listedTitles()).toEqual(["Floor 3 energy"]));
    expect(screen.getByText("Renamed to “Floor 3 energy”.")).toBeTruthy();
    expect(server.conversations.get(conversation.id)?.title).toBe("Floor 3 energy");
    const patches = server.requests.filter((request) => request.method === "PATCH");
    expect(patches.at(-1)?.body).toEqual({ title: "Floor 3 energy" });
  });

  test("Cancel leaves the title as it was and sends nothing", async () => {
    const server = new FakeConversationServer();
    server.add({ title: "Energy this week" });
    open(server);
    await screen.findByRole("table");
    click(within(rowOf("Energy this week")).getByRole("button", { name: "Rename “Energy this week”" }));
    act(() => {
      fireEvent.change(screen.getByRole("textbox", { name: "Conversation title" }), { target: { value: "Other" } });
    });
    click(screen.getByRole("button", { name: "Cancel" }));
    expect(listedTitles()).toEqual(["Energy this week"]);
    expect(server.requests.filter((request) => request.method === "PATCH")).toEqual([]);
  });

  test("archives and unarchives at once, and says so", async () => {
    const server = new FakeConversationServer();
    server.add({ title: "Energy this week" });
    open(server);
    await screen.findByRole("table");
    click(within(rowOf("Energy this week")).getByRole("button", { name: "Archive “Energy this week”" }));
    expect(await screen.findByText("Archived “Energy this week”. You can unarchive it under “Archived”.")).toBeTruthy();
    expect(await screen.findByText("No conversations yet")).toBeTruthy();

    click(screen.getByRole("button", { name: "Archived" }));
    await waitFor(() => expect(listedTitles()).toEqual(["Energy this week"]));
    click(within(rowOf("Energy this week")).getByRole("button", { name: "Unarchive “Energy this week”" }));
    expect(await screen.findByText("Unarchived “Energy this week”. It is back in use.")).toBeTruthy();
    expect(await screen.findByText("No archived conversations")).toBeTruthy();
    const patches = server.requests.filter((request) => request.method === "PATCH").map((request) => request.body);
    expect(patches).toEqual([{ archived: true }, { archived: false }]);
  });

  test("deletes after a confirmation; it is then under Deleted with its restore date, and restoring brings it back", async () => {
    const server = new FakeConversationServer();
    server.now = () => "2026-11-03T10:00:00.000Z";
    const conversation = server.add({ title: "Energy this week" });
    open(server);
    await screen.findByRole("table");
    click(within(rowOf("Energy this week")).getByRole("button", { name: "Delete “Energy this week”" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete “Energy this week”?" });
    expect(dialog.textContent).toContain("restore it for 30 days");
    // The focus starts on Cancel, which closes it without deleting.
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    click(within(dialog).getByRole("button", { name: "Delete conversation" }));
    expect(
      await screen.findByText("Deleted “Energy this week”. You can restore it under “Deleted” for 30 days."),
    ).toBeTruthy();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(await screen.findByText("No conversations yet")).toBeTruthy();

    click(screen.getByRole("button", { name: "Show deleted conversations" }));
    await waitFor(() => expect(listedTitles()).toEqual(["Energy this week"]));
    expect(rowOf("Energy this week").textContent).toContain(when("2026-12-03T10:00:00.000Z"));

    click(within(rowOf("Energy this week")).getByRole("button", { name: "Restore “Energy this week”" }));
    expect(await screen.findByText("Restored “Energy this week”. It is back in use.")).toBeTruthy();
    expect(await screen.findByText("No deleted conversations")).toBeTruthy();
    click(screen.getByRole("button", { name: "In use" }));
    await waitFor(() => expect(listedTitles()).toEqual(["Energy this week"]));
    expect(server.conversations.get(conversation.id)?.deleted_at).toBeUndefined();
  });

  test("deleting a conversation whose question is still running stops the question and deletes it", async () => {
    const server = new FakeConversationServer();
    const conversation = server.add({ title: "Energy this week" });
    conversation.running = true;
    open(server);
    await screen.findByRole("table");
    click(within(rowOf("Energy this week")).getByRole("button", { name: "Delete “Energy this week”" }));
    const dialog = await screen.findByRole("alertdialog");
    click(within(dialog).getByRole("button", { name: "Delete conversation" }));
    expect(
      await screen.findByText("Deleted “Energy this week”. You can restore it under “Deleted” for 30 days."),
    ).toBeTruthy();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(await screen.findByText("No conversations yet")).toBeTruthy();
    expect(server.conversations.get(conversation.id)?.running).toBe(false);
    expect(server.conversations.get(conversation.id)?.deleted_at).toBeDefined();
  });
});
