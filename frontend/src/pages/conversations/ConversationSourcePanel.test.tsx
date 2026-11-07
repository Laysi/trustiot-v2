import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, configure, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath } from "../../app/paths.ts";
import type { Dataset } from "../../dashboard/data.ts";
import { requestDataset, result, SPECS } from "../../dashboard/fixtures.ts";
import { fakeEngine } from "../../test/fake-chart.ts";
import { FakeDashboardServer } from "../../test/fake-dashboards.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { fakeViewport } from "../../test/fake-viewport.ts";
import { renderAt } from "../../test/render.tsx";

// The first test of the file pays for loading the app on a busy machine: wait on conditions with a
// generous bound (CLAUDE.md, "Test time limits").
configure({ asyncUtilTimeout: 15_000 });

const viewports: ReturnType<typeof fakeViewport>[] = [];
afterEach(() => {
  cleanup();
  for (const viewport of viewports.splice(0)) viewport.restore();
});

// The source panel inside a conversation (answers' dashboards and the draft panel).
//
// Decision (docs/design/chat-dashboards/design.md, 「和設計稿不同的實作」, Laysi/trustiot-v2#1175):
// these panels have no 「Ask AI to explain」. The design's conversation does not draw it (only
// `dashboard.js` passes `aiHref`), the answer's own row of 「Ask AI」 buttons under each component and
// the composer already ask in this very conversation, and a button that asked there would add a
// question to the thread from inside a panel that is part of an answer.
const EXPLAIN = { name: "Ask AI to explain" };

const STATS_CHART = { kind: "time_series", title: "Weekly power", dataset: "stats", x: "bucket", y: ["value"] };

function setUp(components: unknown[], devices = new FakeDeviceServer(), datasets: readonly Dataset[] = []) {
  const conversations = new FakeQuestionServer();
  const conversation = conversations.add({ title: "Cooling", messages: [] });
  const dashboards = dashboardsWith(conversation.id, datasets, components);
  const draft = [...dashboards.dashboards.values()][0];
  conversation.messages = [
    { kind: "question", text: "Show me the cooling towers" },
    {
      kind: "answer",
      text: "Here is the dashboard.",
      draft_versions: [{ dashboard_id: draft?.id ?? "", version: 1 }],
    },
  ];
  const viewport = fakeViewport();
  viewports.push(viewport);
  const rendered = renderAt(conversationPath(conversation.id), {
    conversations: conversations.api(),
    dashboards: dashboards.api(),
    devices: devices.api(),
    chartEngine: fakeEngine().engine,
  });
  return { conversations, dashboards, viewport, ...rendered };
}

function dashboardsWith(conversationId: string, datasets: readonly Dataset[], components: unknown[]) {
  const dashboards = new FakeDashboardServer();
  dashboards.results.set(
    "stats",
    result(
      "stats",
      [
        { name: "bucket", type: "time" },
        { name: "value", type: "number" },
      ],
      [["2026-10-26 00:00:00", 1.5]],
    ),
  );
  dashboards.addDashboard({ kind: "draft", conversationId, title: "Cooling tower", versions: [components], datasets });
  return dashboards;
}

/** The preview in the draft panel beside the conversation: the answer's own dashboard has the same name. */
async function draftPreview(): Promise<HTMLElement> {
  const previews = await screen.findAllByRole("region", { name: "Draft preview" });
  const panel = previews.find((preview) => preview.closest("article") === null);
  if (panel === undefined) throw new Error("waiting for the draft panel's preview");
  return panel;
}

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

describe("the source panel inside a conversation", () => {
  test("an answer's panel has the facts and the copy button, and no 「Ask AI to explain」", async () => {
    setUp([SPECS.kpi]);
    const article = await screen.findByRole("article", { name: "TrustIoT's answer" });
    const kpi = await within(article).findByRole("region", { name: "Energy this week" });
    click(within(kpi).getByRole("button", { name: "Source of Energy this week" }));
    const panel = await within(kpi).findByRole("region", { name: "Source" });
    expect(within(panel).getByText("sum(daily.kwh)")).toBeTruthy();
    expect(within(panel).getByText("Rows")).toBeTruthy();
    expect(within(panel).getByRole("button", { name: "Copy the SQL" })).toBeTruthy();
    expect(within(panel).queryByRole("button", EXPLAIN)).toBeNull();
    // The way to ask about the component here is the row under it.
    expect(within(kpi).getByRole("group", { name: "Ask AI about “Energy this week”" })).toBeTruthy();
  });

  test("the draft panel's has none either", async () => {
    setUp([SPECS.kpi]);
    const preview = await draftPreview();
    const kpi = await within(preview).findByRole("region", { name: "Energy this week" });
    click(within(kpi).getByRole("button", { name: "Source of Energy this week" }));
    const panel = await within(kpi).findByRole("region", { name: "Source" });
    expect(within(panel).getByText("sum(daily.kwh)")).toBeTruthy();
    expect(within(panel).queryByRole("button", EXPLAIN)).toBeNull();
  });

  test("a usage query there names its devices from the device list", async () => {
    const devices = new FakeDeviceServer();
    const boiler = devices.addDevice({ name: "Boiler B1" });
    setUp([STATS_CHART], devices, [
      requestDataset("telemetry_usage", "stats", "Power use", {
        series: [{ device_id: boiler.device_id, point: "active_power" }],
        tz: "UTC",
        from: "2026-10-01T00:00:00+08:00",
        to: "2026-10-08T00:00:00+08:00",
        interval: "1w",
      }),
    ]);
    const preview = await draftPreview();
    const card = await within(preview).findByRole("region", { name: "Weekly power" });
    click(within(card).getByRole("button", { name: "Source of Weekly power" }));
    const panel = await within(card).findByRole("region", { name: "Source" });
    await waitFor(() =>
      expect(
        within(within(panel).getByTestId("request-summary"))
          .getAllByRole("listitem")
          .map((item) => item.textContent),
      ).toEqual(["Boiler B1 · , active_power"]),
    );
    expect(within(panel).queryByRole("button", EXPLAIN)).toBeNull();
  });
});
