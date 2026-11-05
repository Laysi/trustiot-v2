import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, configure, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath, dashboardPath } from "../../app/paths.ts";
import { SPECS } from "../../dashboard/fixtures.ts";
import { fakeEngine } from "../../test/fake-chart.ts";
import { FakeDashboardServer, fakeError } from "../../test/fake-dashboards.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { fakeViewport } from "../../test/fake-viewport.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

// The first test of the file pays for loading the app on a busy machine: wait on conditions with a
// generous bound (CLAUDE.md, "Test time limits").
configure({ asyncUtilTimeout: 15_000 });

const viewports: ReturnType<typeof fakeViewport>[] = [];
afterEach(() => {
  cleanup();
  for (const viewport of viewports.splice(0)) viewport.restore();
});

// The page reads the conversation again every 1.5 s while a question runs, so a wait for the end of
// a question is bounded generously (the condition is what is awaited, not the time).
const POLL_WAIT = { timeout: 60_000 };
const SLOW_TEST_MS = 90_000;

/** A conversation whose answer wrote a dashboard with the given components. */
function setUp(components: unknown[], options: { role?: "viewer" | "editor" | "admin" } = {}) {
  const role = options.role ?? "editor";
  const conversations = new FakeQuestionServer();
  const conversation = conversations.add({ title: "Cooling", messages: [] });
  const dashboards = new FakeDashboardServer();
  dashboards.role = role;
  const draft = dashboards.addDashboard({
    kind: "draft",
    conversationId: conversation.id,
    title: "Cooling tower",
    versions: [components],
  });
  conversation.messages = [
    { kind: "question", text: "Show me the cooling towers" },
    {
      kind: "answer",
      text: "Here is the dashboard.",
      draft_versions: [{ dashboard_id: draft.id, version: 1 }],
    },
  ];
  const viewport = fakeViewport();
  viewports.push(viewport);
  const chart = fakeEngine();
  const rendered = renderAt(conversationPath(conversation.id), {
    conversations: conversations.api(),
    dashboards: dashboards.api(),
    chartEngine: chart.engine,
    identity: signedInBrowser({ role }).browser,
  });
  return { conversations, conversation, dashboards, viewport, chart, ...rendered };
}

const component = async (title: string) => {
  const article = await screen.findByRole("article", { name: "TrustIoT's answer" });
  return within(article).findByRole("region", { name: title });
};

const questions = (server: FakeQuestionServer) =>
  server.requests.filter((request) => request.method === "POST" && request.url.pathname.endsWith("/questions"));

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

describe("the 「Ask AI」 row under a component in an answer", () => {
  test("each kind of component offers its own questions, in a group named for the component", async () => {
    setUp([SPECS.kpi, SPECS.timeSeries, SPECS.table]);
    const kpi = await component("Energy this week");
    const row = within(kpi).getByRole("group", { name: "Ask AI about “Energy this week”" });
    expect(
      within(row)
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["Why this number?", "Compare with last week"]);
    const series = await component("Power by line");
    expect(
      within(within(series).getByRole("group", { name: "Ask AI about “Power by line”" }))
        .getAllByRole("button")
        .map((button) => button.textContent),
    ).toEqual(["What stands out?", "Compare with last week", "What caused the peak?"]);
    const table = await component("Meters");
    expect(within(table).getAllByRole("button", { name: /^(What stands out\?|Compare with last week)$/ })).toHaveLength(
      2,
    );
  });

  test(
    "pressing one asks in the same conversation, shows the question while it is answered and holds the buttons until it ends",
    async () => {
      const { conversations, conversation } = setUp([SPECS.kpi]);
      const kpi = await component("Energy this week");
      click(within(kpi).getByRole("button", { name: "Compare with last week" }));

      const text = "Compare “Energy this week” with the same period last week: what is different?";
      await waitFor(() => expect(conversations.latestRun(conversation.id)?.status).toBe("running"));
      // The question went to this conversation, as written, and no other conversation was started.
      expect(questions(conversations).map((request) => [request.url.pathname, request.body])).toEqual([
        [`/api/assistant/v1/conversations/${conversation.id}/questions`, { question: text }],
      ]);
      expect(conversations.conversations.size).toBe(1);
      // The page follows it like a question typed in its composer.
      const pending = await screen.findByRole("list", { name: "Question being answered" });
      expect(within(pending).getByText(text)).toBeTruthy();
      await waitFor(() =>
        expect(within(kpi).getByRole("button", { name: "Why this number?" }).getAttribute("aria-disabled")).toBe(
          "true",
        ),
      );

      act(() => conversations.answer(conversation.id, "Last week was 380 kWh."));
      await waitFor(
        () =>
          expect(
            within(kpi).getByRole("button", { name: "Why this number?" }).getAttribute("aria-disabled"),
          ).toBeNull(),
        POLL_WAIT,
      );
      expect(screen.getByText("Last week was 380 kWh.")).toBeTruthy();
    },
    SLOW_TEST_MS,
  );

  test("a question that cannot be sent is explained under the button that asked", async () => {
    const { conversations, conversation } = setUp([SPECS.kpi]);
    const kpi = await component("Energy this week");
    conversations.failNextOn("ask", fakeError(403, "permission_denied", "error.permission_denied"));
    const button = within(kpi).getByRole("button", { name: "Why this number?" });
    click(button);
    const alert = await within(kpi).findByRole("alert");
    expect(alert.textContent).toBe("The question wasn't sent: You don't have permission to do this.");
    expect(button.getAttribute("aria-describedby")).toBe(alert.id);
    expect(button.parentElement).toBe(alert.parentElement);
    // The page stays where it was, and the other button still works.
    expect(
      within(kpi).getByRole("button", { name: "Compare with last week" }).getAttribute("aria-disabled"),
    ).toBeNull();
    expect(screen.queryByRole("list", { name: "Question being answered" })).toBeNull();
    expect(conversations.latestRun(conversation.id)).toBeUndefined();
  });

  test("a question refused because another is running says so under the button, and no run starts", async () => {
    const { conversations, conversation } = setUp([SPECS.kpi]);
    const kpi = await component("Energy this week");
    conversations.failNextOn("ask", fakeError(409, "conflict", "conversation.question_running"));
    click(within(kpi).getByRole("button", { name: "Compare with last week" }));
    expect((await within(kpi).findByRole("alert")).textContent).toContain(
      "A question in this conversation is still being answered.",
    );
    expect(conversations.latestRun(conversation.id)).toBeUndefined();
    expect(screen.queryByRole("list", { name: "Question being answered" })).toBeNull();
  });

  test("a viewer asks too: asking is read only", async () => {
    setUp([SPECS.kpi], { role: "viewer" });
    const kpi = await component("Energy this week");
    expect(within(kpi).getByRole("button", { name: "Why this number?" })).toBeTruthy();
  });

  test("zooming a time series offers 「Analyze this stretch」, which asks about those times in the user's zone", async () => {
    const { conversations, conversation, chart, viewport } = setUp([SPECS.timeSeries]);
    const series = await component("Power by line");
    viewport.enter(series);
    await waitFor(() => expect(chart.charts[0]?.options.length).toBeGreaterThan(0));
    const instance = chart.charts[0];
    expect(within(series).queryByRole("button", { name: "Analyze this stretch" })).toBeNull();
    await act(async () => {
      // The wall-clock x values: 2026-11-01 15:10 to 16:20.
      if (instance) instance.window = { from: Date.UTC(2026, 10, 1, 15, 10), to: Date.UTC(2026, 10, 1, 16, 20) };
      instance?.emit("datazoom", { start: 10, end: 20 });
    });
    click(await within(series).findByRole("button", { name: "Analyze this stretch" }));
    await waitFor(() => expect(questions(conversations)).toHaveLength(1));
    expect(questions(conversations)[0]?.body).toEqual({
      question:
        "Analyze this stretch: what happened to “Power by line” from 2026-11-01 15:10 to 2026-11-01 16:20 (UTC)? Was there an anomaly or anything else worth noting, and what caused it?",
    });
    expect(conversations.latestRun(conversation.id)?.status).toBe("running");
  });
});

describe("a component outside a conversation", () => {
  test("a saved dashboard's components have no 「Ask AI」 row", async () => {
    const dashboards = new FakeDashboardServer();
    const saved = dashboards.addDashboard({ title: "Energy overview", versions: [[SPECS.kpi, SPECS.timeSeries]] });
    renderAt(dashboardPath(saved.id), {
      dashboards: dashboards.api(),
      chartEngine: fakeEngine().engine,
    });
    await screen.findByRole("region", { name: "Energy this week" });
    expect(screen.queryByRole("group", { name: /^Ask AI about / })).toBeNull();
    expect(screen.queryByRole("button", { name: "Compare with last week" })).toBeNull();
  });
});
