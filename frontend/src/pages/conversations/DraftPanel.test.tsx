import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, configure, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath, dashboardPath } from "../../app/paths.ts";
import { DAILY_ROWS, SPECS } from "../../dashboard/fixtures.ts";
import { dashboardFormat } from "../../dashboard/format.ts";
import { fakeEngine } from "../../test/fake-chart.ts";
import { FakeConversationServer } from "../../test/fake-conversations.ts";
import { FakeDashboardServer, type FakeRole, fakeError } from "../../test/fake-dashboards.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

// The first test of the file pays for loading the app on a busy machine: wait on conditions with a
// generous bound (CLAUDE.md, "Test time limits").
configure({ asyncUtilTimeout: 15_000 });

afterEach(() => {
  cleanup();
});

const format = dashboardFormat("en", "UTC");

/** A conversation and a dashboard server holding drafts of it; the user's role is `role`. */
function setUp(options: { role?: FakeRole } = {}) {
  const conversations = new FakeConversationServer();
  const conversation = conversations.add({ title: "Energy this week" });
  const dashboards = new FakeDashboardServer();
  dashboards.role = options.role ?? "editor";
  const chart = fakeEngine();
  const draft = (
    versions: unknown[][] = [[SPECS.kpi]],
    more: Parameters<FakeDashboardServer["addDashboard"]>[0] = {},
  ) =>
    dashboards.addDashboard({
      kind: "draft",
      conversationId: conversation.id,
      title: "Cooling tower",
      versions,
      ...more,
    });
  const open = (path = conversationPath(conversation.id)) =>
    renderAt(path, {
      conversations: conversations.api(),
      dashboards: dashboards.api(),
      chartEngine: chart.engine,
      identity: signedInBrowser({ role: options.role ?? "editor" }).browser,
    });
  return { conversations, conversation, dashboards, draft, open };
}

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

const SAVE = { name: "Save to dashboard list" };

describe("a conversation's dashboard drafts", () => {
  test("the page shows the conversation's drafts, and the components draw the datasets' values", async () => {
    const { dashboards, draft, open } = setUp();
    const mine = draft();
    // Another conversation's draft is not here.
    dashboards.addDashboard({ kind: "draft", conversationId: "00000000-0000-4000-8000-0000000000c9", title: "Other" });
    open();
    expect(await screen.findByRole("heading", { level: 2, name: "Cooling tower" })).toBeTruthy();
    expect(screen.getByText("Draft · version 1 · not saved")).toBeTruthy();
    const preview = screen.getByRole("region", { name: "Draft preview" });
    const value = await within(await within(preview).findByRole("region", { name: "Energy this week" })).findByTestId(
      "value",
    );
    // The number is the one the dataset holds, queried as the user for the draft itself.
    const total = DAILY_ROWS.reduce((sum, row) => sum + (typeof row[2] === "number" ? row[2] : 0), 0);
    await waitFor(() => expect(value.textContent).toBe(format.number(total)));
    expect(
      dashboards.requests.some((request) => request.url.pathname.includes(`/dashboards/${mine.id}/dataset-runs`)),
    ).toBe(true);
    expect(screen.queryByText("Other")).toBeNull();
    // Nothing is saved by looking.
    expect(dashboards.requests.some((request) => request.url.pathname.endsWith("/publish"))).toBe(false);
  });

  test("a draft's custom page item is drawn by the same sandbox frame as on the dashboard page", async () => {
    const { draft, open } = setUp();
    draft([[SPECS.kpi, { kind: "custom_page" }]], { page: '<div id="chart"></div><script>tq.ready()</script>' });
    open();
    const frame = await waitFor(() => {
      const found = document.querySelector("iframe");
      if (found === null) throw new Error("waiting for the sandbox frame");
      return found;
    });
    expect(frame.getAttribute("src")).toMatch(/^\/sandbox\/v1\/sandbox\.html(#|\?|$)/);
    expect((frame.getAttribute("sandbox") ?? "").split(/\s+/)).not.toContain("allow-same-origin");
    const custom = document.querySelector('[data-component="custom_page"]') as HTMLElement;
    expect(within(custom).getByRole("heading", { level: 3, name: "Cooling tower" })).toBeTruthy();
    expect(custom.contains(frame)).toBe(true);
  });

  test("without a draft the page is the plain conversation", async () => {
    const { open } = setUp();
    open();
    await screen.findByRole("heading", { level: 1, name: "Energy this week" });
    await waitFor(() => expect(screen.queryByRole("region", { name: "Draft preview" })).toBeNull());
    expect(screen.queryByRole("button", SAVE)).toBeNull();
  });

  test("an editor saves the draft: the dashboard is in the list, and the link opens its page", async () => {
    const { dashboards, draft, open } = setUp({ role: "editor" });
    const mine = draft([[SPECS.table], [SPECS.kpi]]);
    open();
    await screen.findByText("Draft · version 2 · not saved");
    click(screen.getByRole("button", SAVE));
    const done = await screen.findByText("Saved as version 1 in the dashboard list");
    expect(done).toBeTruthy();
    expect(screen.getByText("Published · version 1")).toBeTruthy();
    // The version the user looked at is the one saved, and saving twice is not offered.
    const publish = dashboards.requests.filter((request) => request.url.pathname.endsWith("/publish"));
    expect(publish.map((request) => request.body)).toEqual([{ version: 2 }]);
    expect(publish[0]?.url.pathname).toBe(`/api/assistant/v1/dashboards/${mine.id}/publish`);
    expect((screen.getByRole("button", SAVE) as HTMLButtonElement).disabled).toBe(true);
    const saved = [...dashboards.dashboards.values()].filter((dashboard) => dashboard.kind === "saved");
    expect(saved).toHaveLength(1);

    const link = screen.getByRole("link", { name: "Open the dashboard" });
    expect(link.getAttribute("href")).toBe(dashboardPath(saved[0]?.id as string));
    click(link);
    expect(await screen.findByRole("heading", { level: 1, name: "Cooling tower" })).toBeTruthy();
    expect(await screen.findByText("Published · version 1")).toBeTruthy();
  });

  test("an admin saves it too, and a draft taken from a saved dashboard becomes its next version", async () => {
    const { dashboards, draft, open } = setUp({ role: "admin" });
    const base = dashboards.addDashboard({ title: "Cooling tower", versions: [[SPECS.table]] });
    // Written on the saved dashboard's version 1, which is still its latest.
    const mine = draft([[SPECS.table]], { saved: { id: base.id, version: 1 } });
    open();
    expect(await screen.findByText(/Saving adds a new version to that dashboard/)).toBeTruthy();
    expect(screen.getByRole("link", { name: "Open that dashboard" }).getAttribute("href")).toBe(dashboardPath(base.id));
    click(screen.getByRole("button", SAVE));
    expect(await screen.findByText("Saved as version 2 in the dashboard list")).toBeTruthy();
    expect(base.versions).toHaveLength(2);
    expect(base.versions[1]?.origin).toBe("publish");
    expect(mine.saved).toEqual({ id: base.id, version: 2 });
    // The draft's saved dashboard is read again: the list shows its new version.
    expect(screen.getByRole("link", { name: "Open the dashboard" }).getAttribute("href")).toBe(dashboardPath(base.id));
  });

  test("a viewer sees the draft and a note, and no way to save it", async () => {
    const { dashboards, draft, open } = setUp({ role: "viewer" });
    draft();
    open();
    expect(await screen.findByRole("heading", { level: 2, name: "Cooling tower" })).toBeTruthy();
    expect(screen.getByText(/Only editors and admins can save drafts/)).toBeTruthy();
    expect(screen.queryByRole("button", SAVE)).toBeNull();
    expect(screen.queryByText("Save to dashboard list")).toBeNull();
    expect(dashboards.requests.some((request) => request.url.pathname.endsWith("/publish"))).toBe(false);
  });

  test("a draft based on an older version of the saved dashboard says so and overwrites nothing", async () => {
    const { dashboards, draft, open } = setUp();
    // Someone published a second version after the draft was written on the first.
    const base = dashboards.addDashboard({ title: "Cooling tower", versions: [[SPECS.table], [SPECS.bar]] });
    draft([[SPECS.kpi]], { saved: { id: base.id, version: 1 } });
    open();
    await screen.findByText("Draft · version 1 · not saved");
    click(screen.getByRole("button", SAVE));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The draft wasn't saved.");
    // The server's text, translated, with its version; never the key or the raw body.
    expect(alert.textContent).toContain("not based on the saved dashboard's latest version (version 2)");
    expect(alert.textContent).not.toContain("dashboard.saved_changed");
    expect(base.versions).toHaveLength(2);
    // Nothing was saved, so it can be tried again once the draft is merged.
    expect((screen.getByRole("button", SAVE) as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByText(/Saved as version/)).toBeNull();
  });

  test("a refusal for permission or an archived dashboard is translated, not the server's text", async () => {
    const { dashboards, draft, open } = setUp();
    draft();
    open();
    await screen.findByText("Draft · version 1 · not saved");
    dashboards.failNext("publish", fakeError(403, "permission_denied", "dashboard.editor_required"));
    click(screen.getByRole("button", SAVE));
    expect((await screen.findByRole("alert")).textContent).toContain(
      "Only editors and admins can publish, restore or archive dashboards.",
    );
    dashboards.failNext("publish", fakeError(409, "conflict", "dashboard.archived"));
    click(screen.getByRole("button", SAVE));
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toContain("This dashboard is archived. Unarchive it first."),
    );
    // A key this build does not know shows the code's message, never the key.
    dashboards.failNext("publish", fakeError(429, "rate_limited", "dashboard.unheard_of"));
    click(screen.getByRole("button", SAVE));
    await waitFor(() => expect(screen.getByRole("alert").textContent).not.toContain("dashboard.unheard_of"));
  });

  test("the panel waits for the draft and says so; a failure says why and tries again", async () => {
    const { dashboards, draft, open } = setUp();
    draft();
    dashboards.failNext("dashboard", fakeError(503, "unavailable", "error.unavailable"));
    open();
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The draft couldn't be loaded.");
    click(within(alert).getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { level: 2, name: "Cooling tower" })).toBeTruthy();
  });

  test("a failure to list the drafts leaves the conversation and offers to try again", async () => {
    const { dashboards, draft, open } = setUp();
    draft();
    dashboards.failNext("drafts", fakeError(503, "unavailable", "error.unavailable"));
    open();
    expect(await screen.findByRole("heading", { level: 1, name: "Energy this week" })).toBeTruthy();
    const alert = await screen.findByText(/This conversation's drafts couldn't be loaded/);
    click(within(alert.closest('[role="alert"]') as HTMLElement).getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { level: 2, name: "Cooling tower" })).toBeTruthy();
  });

  test("a saved version stays saved when another draft is chosen and chosen again", async () => {
    const { dashboards, draft, open } = setUp();
    draft([[SPECS.kpi]], { title: "Older draft", updatedAt: "2026-11-01T08:00:00Z" });
    draft([[SPECS.table]], { title: "Newer draft", updatedAt: "2026-11-01T09:00:00Z" });
    open();
    await screen.findByRole("heading", { level: 2, name: "Newer draft" });
    click(screen.getByRole("button", SAVE));
    await screen.findByText("Saved as version 1 in the dashboard list");
    const choose = screen.getByRole("combobox", { name: "Draft" }) as HTMLSelectElement;
    const [newer, older] = [...choose.options].map((option) => option.value);
    act(() => {
      fireEvent.change(choose, { target: { value: older } });
    });
    await screen.findByRole("heading", { level: 2, name: "Older draft" });
    expect(screen.queryByText(/Saved as version/)).toBeNull();
    expect((screen.getByRole("button", SAVE) as HTMLButtonElement).disabled).toBe(false);
    act(() => {
      fireEvent.change(choose, { target: { value: newer } });
    });
    await screen.findByText("Saved as version 1 in the dashboard list");
    expect((screen.getByRole("button", SAVE) as HTMLButtonElement).disabled).toBe(true);
    expect(dashboards.requests.filter((request) => request.url.pathname.endsWith("/publish"))).toHaveLength(1);
  });

  test("with several drafts the latest is shown first and the others can be picked by title", async () => {
    const { draft, open } = setUp();
    draft([[SPECS.kpi]], { title: "Older draft", updatedAt: "2026-11-01T08:00:00Z" });
    draft([[SPECS.table]], { title: "Newer draft", updatedAt: "2026-11-01T09:00:00Z" });
    open();
    expect(await screen.findByRole("heading", { level: 2, name: "Newer draft" })).toBeTruthy();
    const choose = screen.getByRole("combobox", { name: "Draft" }) as HTMLSelectElement;
    expect([...choose.options].map((option) => option.textContent)).toEqual([
      "Newer draft · version 1",
      "Older draft · version 1",
    ]);
    act(() => {
      fireEvent.change(choose, { target: { value: choose.options[1]?.value } });
    });
    expect(await screen.findByRole("heading", { level: 2, name: "Older draft" })).toBeTruthy();
  });
});
