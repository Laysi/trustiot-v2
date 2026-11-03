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

describe("the home page's recent conversations", () => {
  test("lists the five most recently active, each opening its page, and links to the whole history", async () => {
    const server = new FakeConversationServer();
    for (let day = 1; day <= 7; day += 1) {
      server.add({ title: `Day ${day}`, activityAt: `2026-11-0${day}T08:00:00Z` });
    }
    server.add({ title: "Archived", activityAt: "2026-11-09T08:00:00Z", archivedAt: "2026-11-09T09:00:00Z" });
    const { router } = renderAt("/", { conversations: server.api() });
    const section = await screen.findByRole("region", { name: "Recent conversations" });
    const links = within(section).getAllByRole("link");
    expect(links.map((link) => link.textContent)).toEqual([
      `Day 7Last activity ${formatDateTime("2026-11-07T08:00:00Z", "en", "UTC")}`,
      expect.stringContaining("Day 6"),
      expect.stringContaining("Day 5"),
      expect.stringContaining("Day 4"),
      expect.stringContaining("Day 3"),
      "All conversations",
    ]);
    expect(links.at(-1)?.getAttribute("href")).toBe("/conversations");
    // One request for the five, without the archived ones.
    expect(server.listRequests()).toEqual(["/api/assistant/v1/conversations?limit=5"]);
    // The home page's own content stays.
    expect(screen.getByRole("heading", { level: 1, name: "Home" })).toBeTruthy();

    const day7 = [...server.conversations.values()].find((conversation) => conversation.title === "Day 7");
    act(() => {
      fireEvent.click(links[0] as HTMLElement);
    });
    expect(await screen.findByRole("heading", { level: 1, name: "Day 7" })).toBeTruthy();
    expect(router.state.location.pathname).toBe(conversationPath(day7?.id ?? ""));
  });

  test("is not there when there are no conversations, nor while they load", async () => {
    const server = new FakeConversationServer();
    server.add({ title: "Archived only", archivedAt: "2026-11-01T00:00:00Z" });
    server.hold(["list"]);
    const { queryClient } = renderAt("/", { conversations: server.api() });
    expect(await screen.findByRole("heading", { level: 1, name: "Home" })).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Recent conversations" })).toBeNull();
    act(() => server.release());
    await waitFor(() => expect(queryClient.isFetching()).toBe(0));
    expect(server.listRequests()).toHaveLength(1);
    expect(screen.queryByRole("region", { name: "Recent conversations" })).toBeNull();
    expect(screen.queryByText("All conversations")).toBeNull();
  });

  test("a failed read says so in one line and retries", async () => {
    const server = new FakeConversationServer();
    server.add({ title: "Energy this week" });
    server.failNext("list", fakeError(503, "unavailable", "error.unavailable"));
    renderAt("/", { conversations: server.api() });
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Couldn't load your recent conversations");
    act(() => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    const section = await screen.findByRole("region", { name: "Recent conversations" });
    expect(within(section).getByText("Energy this week")).toBeTruthy();
  });
});
