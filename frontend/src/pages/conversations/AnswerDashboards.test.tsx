import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, configure, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath, dashboardPath } from "../../app/paths.ts";
import { DAILY_ROWS, SPECS } from "../../dashboard/fixtures.ts";
import { dashboardFormat } from "../../dashboard/format.ts";
import { fakeEngine } from "../../test/fake-chart.ts";
import { FakeConversationServer } from "../../test/fake-conversations.ts";
import { type FakeDashboard, FakeDashboardServer, type FakeRole, fakeError } from "../../test/fake-dashboards.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { fakeViewport } from "../../test/fake-viewport.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { shownDraftVersions } from "./AnswerDashboards.tsx";

// The first test of the file pays for loading the app on a busy machine: wait on conditions with a
// generous bound (CLAUDE.md, "Test time limits").
configure({ asyncUtilTimeout: 15_000 });

const viewports: ReturnType<typeof fakeViewport>[] = [];
afterEach(() => {
  cleanup();
  for (const viewport of viewports.splice(0)) viewport.restore();
});

const format = dashboardFormat("en", "UTC");
const SAVE = { name: "Save to dashboard list" };
const OTHER_USER = "00000000-0000-4000-8000-0000000000b2";

/**
 * A conversation whose answer wrote drafts, a dashboard server holding them, and a viewport
 * nothing is near until the test says so. `answer(...)` names the versions the answer wrote.
 */
function setUp(options: { role?: FakeRole } = {}) {
  const role = options.role ?? "editor";
  const conversations = new FakeConversationServer();
  const conversation = conversations.add({ title: "Cooling", messages: [] });
  const dashboards = new FakeDashboardServer();
  dashboards.role = role;
  const viewport = fakeViewport();
  viewports.push(viewport);
  const chart = fakeEngine();
  const draft = (title: string, versions: unknown[][], more: Parameters<FakeDashboardServer["addDashboard"]>[0] = {}) =>
    dashboards.addDashboard({ kind: "draft", conversationId: conversation.id, title, versions, ...more });
  const answer = (...written: [FakeDashboard, number][]) => {
    conversation.messages = [
      { kind: "question", text: "Show me the cooling towers" },
      {
        kind: "answer",
        text: "Here is the dashboard.",
        draft_versions: written.map(([dashboard, version]) => ({ dashboard_id: dashboard.id, version })),
      },
    ];
  };
  const open = () =>
    renderAt(conversationPath(conversation.id), {
      conversations: conversations.api(),
      dashboards: dashboards.api(),
      chartEngine: chart.engine,
      identity: signedInBrowser({ role }).browser,
    });
  return { conversations, conversation, dashboards, viewport, draft, answer, open };
}

const theAnswer = () => screen.findByRole("article", { name: "TrustIoT's answer" });

/** The dashboards drawn in the answer, by title, in order. */
function embedded(article: HTMLElement): string[] {
  return [...article.querySelectorAll("[data-answer-dashboard]")].map(
    (embed) => document.getElementById(embed.getAttribute("aria-labelledby") ?? "")?.textContent ?? "",
  );
}

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

describe("shownDraftVersions", () => {
  test("keeps the last version of each draft, in the order the drafts were first written", () => {
    expect(
      shownDraftVersions([
        { dashboard_id: "a", version: 1 },
        { dashboard_id: "b", version: 1 },
        { dashboard_id: "a", version: 2 },
        { dashboard_id: "a", version: 3 },
      ]),
    ).toEqual([
      { dashboard_id: "a", version: 3 },
      { dashboard_id: "b", version: 1 },
    ]);
    expect(shownDraftVersions([])).toEqual([]);
    expect(shownDraftVersions(null)).toEqual([]);
    expect(shownDraftVersions(undefined)).toEqual([]);
  });
});

describe("the dashboards an answer wrote", () => {
  test("the answer draws the dashboard, and its numbers are the datasets' values", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp();
    const mine = draft("Cooling tower", [[SPECS.kpi]]);
    answer([mine, 1]);
    open();
    const article = await theAnswer();
    const embed = await within(article).findByRole("region", { name: "Cooling tower" });
    viewport.enter(embed);
    const value = await within(await within(article).findByRole("region", { name: "Energy this week" })).findByTestId(
      "value",
    );
    const total = DAILY_ROWS.reduce((sum, row) => sum + (typeof row[2] === "number" ? row[2] : 0), 0);
    await waitFor(() => expect(value.textContent).toBe(format.number(total)));
    expect(within(article).getByText("Draft · version 1 · not saved")).toBeTruthy();
    // The answer's text is still its own, and the dashboard was read as the version it wrote.
    expect(within(article).getByText("Here is the dashboard.")).toBeTruthy();
    expect(
      dashboards.requests.some(
        (request) =>
          request.url.pathname === `/api/assistant/v1/dashboards/${mine.id}` && request.url.search === "?version=1",
      ),
    ).toBe(true);
    // Nothing is saved by looking.
    expect(dashboards.requests.some((request) => request.url.pathname.endsWith("/publish"))).toBe(false);
  });

  test("only the last version of each draft is drawn, the drafts in the order they were first written", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp();
    const first = draft("First tower", [[SPECS.kpi], [SPECS.table], [SPECS.bar]]);
    const second = draft("Second tower", [[SPECS.table]]);
    answer([first, 1], [second, 1], [first, 2], [first, 3]);
    open();
    const article = await theAnswer();
    await within(article).findByRole("region", { name: "First tower" });
    await within(article).findByRole("region", { name: "Second tower" });
    viewport.enter();
    expect(embedded(article)).toEqual(["First tower", "Second tower"]);
    // The first draft is the version 3 (a bar chart), not the versions it went through.
    await within(article).findByRole("region", { name: "Energy by day" });
    expect(within(article).queryByRole("region", { name: "Energy this week" })).toBeNull();
    const reads = dashboards.requests
      .filter((request) => request.url.search.startsWith("?version="))
      .map(
        (request) => `${request.url.pathname.split("/").at(-1) === first.id ? "first" : "second"}${request.url.search}`,
      );
    // (The draft panel reads the same version too: what matters is that no other version is read.)
    expect([...new Set(reads.filter((read) => read.startsWith("first")))]).toEqual(["first?version=3"]);
    expect([...new Set(reads.filter((read) => read.startsWith("second")))]).toEqual(["second?version=1"]);
  });

  test("a version shown in both the answer and the draft panel runs each of its datasets once", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp();
    // A draft of this conversation: the panel draws it too, without waiting for the viewport.
    const mine = draft("Cooling tower", [[SPECS.kpi, SPECS.table]]);
    answer([mine, 1]);
    open();
    const article = await theAnswer();
    const embed = await within(article).findByRole("region", { name: "Cooling tower" });
    viewport.enter(embed);
    await within(article).findByRole("region", { name: "Energy this week" });
    await waitFor(() => expect(dashboards.running).toBe(0));
    const runs = dashboards.runs();
    expect(runs.length).toBeGreaterThan(0);
    const keys = runs.map((run) => `${run.version}:${run.dataset}`);
    expect(keys.sort()).toEqual([...new Set(keys)].sort());
  });

  test("an answer that wrote nothing, and a question, have no dashboard", async () => {
    const { conversation, dashboards, open } = setUp();
    conversation.messages = [
      { kind: "question", text: "Hi" },
      { kind: "answer", text: "Hello.", draft_versions: [] },
      { kind: "question", text: "And?" },
      { kind: "answer", text: "An old answer." },
    ];
    open();
    await screen.findByText("An old answer.");
    expect(dashboards.requests.filter((request) => request.url.pathname.includes("/dashboards/"))).toHaveLength(0);
  });

  test("datasets run only when the dashboard is near the viewport, two at a time for the whole page", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp();
    // Drafts of another conversation: the panel does not run their datasets, only the answer does.
    const first = draft("First tower", [[SPECS.kpi, SPECS.table]], {
      conversationId: "00000000-0000-4000-8000-0000000000c9",
    });
    const second = draft("Second tower", [[SPECS.timeSeries, SPECS.bar]], {
      conversationId: "00000000-0000-4000-8000-0000000000c9",
    });
    answer([first, 1], [second, 1]);
    dashboards.hold();
    open();
    const article = await theAnswer();
    const embeds = [
      await within(article).findByRole("region", { name: "First tower" }),
      await within(article).findByRole("region", { name: "Second tower" }),
    ];
    // Drawn, with their loading placeholders, and not asked for any data.
    expect(viewport.margins.length).toBeGreaterThan(0);
    expect(dashboards.runs()).toHaveLength(0);
    viewport.enter(embeds[0] as HTMLElement);
    // The assistant runs two datasets of a tenant at once: the page does not ask for more.
    await waitFor(() => expect(dashboards.running).toBe(2));
    const ranFor = (id: string) =>
      dashboards.requests.filter(
        (request) => request.url.pathname.endsWith("/dataset-runs") && request.url.pathname.includes(id),
      ).length;
    expect(ranFor(first.id)).toBe(2);
    expect(ranFor(second.id)).toBe(0);
    // The second dashboard comes near: its runs wait for the first's, they do not add to them.
    viewport.enter(embeds[1] as HTMLElement);
    expect(dashboards.running).toBe(2);
    dashboards.release();
    await waitFor(() => expect(ranFor(second.id)).toBeGreaterThan(0));
    await waitFor(() => expect(dashboards.running).toBe(0));
    expect(ranFor(first.id) + ranFor(second.id)).toBe(8);
    expect(dashboards.maxRunning).toBe(2);
  });

  test("saving from the answer shows as saved in the answer and in the draft panel", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp();
    const mine = draft("Cooling tower", [[SPECS.table], [SPECS.kpi]]);
    answer([mine, 1], [mine, 2]);
    open();
    const article = await theAnswer();
    const panelTitle = await screen.findByRole("heading", { level: 2, name: "Cooling tower" });
    const panel = panelTitle.closest("section") as HTMLElement;
    await within(panel).findByText("Draft · version 2 · not saved");
    viewport.enter();
    click(await within(article).findByRole("button", SAVE));
    expect(await within(article).findByText("Saved as version 1 in the dashboard list")).toBeTruthy();
    expect(await within(panel).findByText("Saved as version 1 in the dashboard list")).toBeTruthy();
    expect(within(article).getByText("Published · version 1")).toBeTruthy();
    expect(within(panel).getByText("Published · version 1")).toBeTruthy();
    // The version the answer showed, once, and neither place offers it again.
    const publish = dashboards.requests.filter((request) => request.url.pathname.endsWith("/publish"));
    expect(publish.map((request) => request.body)).toEqual([{ version: 2 }]);
    expect((within(article).getByRole("button", SAVE) as HTMLButtonElement).disabled).toBe(true);
    expect((within(panel).getByRole("button", SAVE) as HTMLButtonElement).disabled).toBe(true);
    // The link goes to the saved dashboard's page, and the dashboard is in the list.
    const saved = [...dashboards.dashboards.values()].filter((dashboard) => dashboard.kind === "saved");
    expect(saved).toHaveLength(1);
    expect(within(article).getByRole("link", { name: "Open the dashboard" }).getAttribute("href")).toBe(
      dashboardPath(saved[0]?.id as string),
    );
    // The place that saved takes the focus; the other does not.
    expect(document.activeElement).toBe(within(article).getByRole("link", { name: "Open the dashboard" }));
  });

  test("saving from the draft panel shows as saved in the answer", async () => {
    const { draft, answer, viewport, open } = setUp();
    const mine = draft("Cooling tower", [[SPECS.kpi]]);
    answer([mine, 1]);
    open();
    const article = await theAnswer();
    const panel = (await screen.findByRole("heading", { level: 2, name: "Cooling tower" })).closest(
      "section",
    ) as HTMLElement;
    viewport.enter();
    await within(article).findByRole("button", SAVE);
    click(within(panel).getByRole("button", SAVE));
    expect(await within(article).findByText("Saved as version 1 in the dashboard list")).toBeTruthy();
    expect((within(article).getByRole("button", SAVE) as HTMLButtonElement).disabled).toBe(true);
    expect(document.activeElement).toBe(within(panel).getByRole("link", { name: "Open the dashboard" }));
  });

  test("a draft taken from a saved dashboard is saved into that dashboard", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp();
    const base = dashboards.addDashboard({ title: "Cooling tower", versions: [[SPECS.table]] });
    const mine = draft("Cooling tower", [[SPECS.kpi]], { saved: { id: base.id, version: 1 } });
    answer([mine, 1]);
    open();
    const article = await theAnswer();
    viewport.enter();
    click(await within(article).findByRole("button", SAVE));
    expect(await within(article).findByText("Saved as version 2 in the dashboard list")).toBeTruthy();
    expect(base.versions).toHaveLength(2);
    expect(within(article).getByRole("link", { name: "Open the dashboard" }).getAttribute("href")).toBe(
      dashboardPath(base.id),
    );
  });

  test("a viewer sees the dashboard and no way to save it", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp({ role: "viewer" });
    const mine = draft("Cooling tower", [[SPECS.kpi]]);
    answer([mine, 1]);
    open();
    const article = await theAnswer();
    const embed = await within(article).findByRole("region", { name: "Cooling tower" });
    viewport.enter(embed);
    await within(article).findByRole("region", { name: "Energy this week" });
    expect(within(article).queryByRole("button", SAVE)).toBeNull();
    expect(within(article).queryByText("Save to dashboard list")).toBeNull();
    expect(dashboards.requests.some((request) => request.url.pathname.endsWith("/publish"))).toBe(false);
  });

  test("when the server says can_publish is false, the answer has no save button", async () => {
    const { draft, answer, viewport, open } = setUp({ role: "editor" });
    const theirs = draft("Cooling tower", [[SPECS.kpi]], { createdBy: OTHER_USER });
    answer([theirs, 1]);
    open();
    const article = await theAnswer();
    const embed = await within(article).findByRole("region", { name: "Cooling tower" });
    viewport.enter(embed);
    await within(article).findByRole("region", { name: "Energy this week" });
    expect(within(article).queryByRole("button", SAVE)).toBeNull();
  });

  test("a failing component or a dataset that fails affects only itself", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp();
    const mine = draft("Cooling tower", [[SPECS.kpi, SPECS.table]]);
    answer([mine, 1]);
    dashboards.failNext("run:readings", fakeError(503, "unavailable", "dashboard.dataset_failed"));
    open();
    const article = await theAnswer();
    viewport.enter();
    const kpi = await within(article).findByRole("region", { name: "Energy this week" });
    await within(kpi).findByTestId("value");
    const table = within(article).getByRole("region", { name: "Meters" });
    await waitFor(() => expect(within(table).getByRole("button", { name: "Run again" })).toBeTruthy());
    // The rest of the answer, and its save button, work.
    expect(within(article).getByText("Here is the dashboard.")).toBeTruthy();
    expect(within(article).getByRole("button", SAVE)).toBeTruthy();
  });

  test("a version that cannot be read shows a note in its place, and the rest of the answer works", async () => {
    const { draft, answer, conversation, viewport, open } = setUp();
    const mine = draft("Cooling tower", [[SPECS.kpi]]);
    answer([mine, 1]);
    // A draft the user cannot read: 404 for the dashboard API.
    const message = conversation.messages[1];
    if (message === undefined) throw new Error("no answer");
    message.draft_versions = [
      { dashboard_id: "00000000-0000-4000-8000-0000000000dd", version: 1 },
      ...(message.draft_versions ?? []),
    ];
    open();
    const article = await theAnswer();
    const note = await within(article).findByText(/A dashboard this answer wrote can't be shown\./);
    expect(note.textContent).not.toContain("not_found");
    viewport.enter();
    expect(await within(article).findByRole("region", { name: "Energy this week" })).toBeTruthy();
    expect(within(article).getByText("Here is the dashboard.")).toBeTruthy();
    expect(within(article).getByRole("button", SAVE)).toBeTruthy();
  });

  test("a refusal because the saved dashboard changed adds which version that is, in the answer and in the panel", async () => {
    const { dashboards, draft, answer, viewport, open } = setUp();
    // Someone saved a second version after the draft was written on the first.
    const base = dashboards.addDashboard({ title: "Cooling tower", versions: [[SPECS.table], [SPECS.bar]] });
    const mine = draft("Cooling tower", [[SPECS.kpi]], { saved: { id: base.id, version: 1 } });
    answer([mine, 1]);
    open();
    const article = await theAnswer();
    const panel = (await screen.findByRole("heading", { level: 2, name: "Cooling tower" })).closest(
      "section",
    ) as HTMLElement;
    viewport.enter();
    click(await within(article).findByRole("button", SAVE));
    const refusal = await within(article).findByRole("alert");
    expect(refusal.textContent).toContain("The draft wasn't saved.");
    expect(refusal.textContent).toContain("not based on the saved dashboard's latest version (version 2)");
    expect(refusal.textContent).toContain("If you just saved this version, version 2 of the saved dashboard is it.");
    expect(refusal.textContent).not.toContain("dashboard.saved_changed");
    click(within(panel).getByRole("button", SAVE));
    const panelRefusal = await within(panel).findByRole("alert");
    expect(panelRefusal.textContent).toContain(
      "If you just saved this version, version 2 of the saved dashboard is it.",
    );
    // Nothing was saved, and it can be tried again.
    expect(base.versions).toHaveLength(2);
    expect((within(article).getByRole("button", SAVE) as HTMLButtonElement).disabled).toBe(false);
    // Another refusal has no such sentence.
    dashboards.failNext("publish", fakeError(409, "conflict", "dashboard.archived"));
    click(within(article).getByRole("button", SAVE));
    await waitFor(() =>
      expect(within(article).getByRole("alert").textContent).toContain("This dashboard is archived."),
    );
    expect(within(article).getByRole("alert").textContent).not.toContain("If you just saved this version");
  });
});

describe("after a question the page asked", () => {
  // The page reads the conversation again every 1.5 s while a question runs, so the wait for the
  // answer is bounded generously (the condition is what is awaited, not the time).
  const POLL_WAIT = { timeout: 60_000 };
  const SLOW_TEST_MS = 90_000;

  test(
    "the draft the answer wrote is drawn in the answer and shown in the draft panel, without a reload",
    async () => {
      const server = new FakeQuestionServer();
      const conversation = server.add({
        title: "Cooling",
        messages: [
          { kind: "question", text: "How are the cooling towers?" },
          { kind: "answer", text: "All three are running." },
        ],
      });
      const dashboards = new FakeDashboardServer();
      dashboards.role = "editor";
      const viewport = fakeViewport();
      viewports.push(viewport);
      renderAt(conversationPath(conversation.id), {
        conversations: server.api(),
        dashboards: dashboards.api(),
        chartEngine: fakeEngine().engine,
        identity: signedInBrowser({ role: "editor" }).browser,
      });
      // No drafts yet: no panel, and the answer has nothing to save.
      await theAnswer();
      expect(screen.queryByRole("button", SAVE)).toBeNull();

      const input = await screen.findByRole("textbox", { name: "Keep asking TrustIoT" });
      act(() => {
        fireEvent.change(input, { target: { value: "Make me a dashboard of them" } });
      });
      click(screen.getByRole("button", { name: "Send" }));
      await waitFor(() => expect(server.latestRun(conversation.id)?.status).toBe("running"));

      // The question writes a draft and its answer names it.
      const written = dashboards.addDashboard({
        kind: "draft",
        conversationId: conversation.id,
        title: "Cooling tower",
        versions: [[SPECS.kpi]],
      });
      act(() => {
        server.answer(conversation.id, "Here is the dashboard.");
        const answer = conversation.messages.at(-1);
        if (answer?.kind === "answer") answer.draft_versions = [{ dashboard_id: written.id, version: 1 }];
      });

      // One button in the answer, one in the panel the drafts were read again for.
      await waitFor(() => expect(screen.getAllByRole("button", SAVE)).toHaveLength(2), POLL_WAIT);
      const answers = screen.getAllByRole("article", { name: "TrustIoT's answer" });
      expect(embedded(answers[answers.length - 1] as HTMLElement)).toEqual(["Cooling tower"]);
    },
    SLOW_TEST_MS,
  );
});
