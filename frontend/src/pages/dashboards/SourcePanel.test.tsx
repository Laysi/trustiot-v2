import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, configure, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { conversationPath, dashboardPath } from "../../app/paths.ts";
import { requestDataset, result, SPECS } from "../../dashboard/fixtures.ts";
import { fakeEngine } from "../../test/fake-chart.ts";
import { FakeDashboardServer } from "../../test/fake-dashboards.ts";
import { FakeDeviceServer, fakeError } from "../../test/fake-devices.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { renderAt } from "../../test/render.tsx";
import { until } from "../../test/until.ts";

// The first test of the file pays for loading the app on a busy machine: wait on conditions with a
// generous bound (CLAUDE.md, "Test time limits").
configure({ asyncUtilTimeout: 15_000 });

afterEach(() => {
  cleanup();
});

const UNLISTED_DEVICE = "0b8a2e91-5c46-4d17-8f03-6a9d7e1c2b48";

/** A chart over the statistics dataset `stats`, so its card has a source button. */
const STATS_CHART = { kind: "time_series", title: "Weekly power", dataset: "stats", x: "bucket", y: ["value"] };

function click(element: HTMLElement) {
  act(() => {
    fireEvent.click(element);
  });
}

/** The panel of the card titled `title`, opened from the card's header button. */
async function openSource(title: string): Promise<HTMLElement> {
  const card = (await screen.findByRole("heading", { level: 3, name: title })).closest("section") as HTMLElement;
  click(within(card).getByRole("button", { name: `Source of ${title}` }));
  return within(card).findByRole("region", { name: "Source" });
}

function setUp(options: { series?: { device_id: string; point: string }[]; devices?: FakeDeviceServer } = {}) {
  const devices = options.devices ?? new FakeDeviceServer();
  const dashboards = new FakeDashboardServer();
  const conversations = new FakeQuestionServer();
  const stats = requestDataset("telemetry_statistics", "stats", "Power statistics", {
    series: options.series ?? [],
    tz: "Asia/Taipei",
    period: "last_7_days",
    interval: "1d",
  });
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
  const dashboard = dashboards.addDashboard({
    title: "Energy overview",
    versions: [[SPECS.timeSeries, STATS_CHART, SPECS.kpi]],
    datasets: [stats],
  });
  const chart = fakeEngine();
  const rendered = renderAt(`${dashboardPath(dashboard.id)}?version=1`, {
    dashboards: dashboards.api(),
    conversations: conversations.api(),
    devices: devices.api(),
    chartEngine: chart.engine,
  });
  return { devices, dashboards, conversations, dashboard, ...rendered };
}

const asked = (server: FakeQuestionServer) =>
  server.requests.filter((request) => request.method === "POST" && request.url.pathname.endsWith("/questions"));

describe("「Ask AI to explain」 in the dashboard page's source panel", () => {
  test("opens a new conversation that asks about the dataset and the dashboard by name, with a way back", async () => {
    const { conversations, dashboard, router } = setUp();
    const panel = await openSource("Power by line");
    click(within(panel).getByRole("button", { name: "Ask AI to explain" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\/[0-9a-f-]{36}$/));
    expect(asked(conversations).map((request) => request.body)).toEqual([
      {
        question:
          "Explain where “Line power” on the dashboard “Energy overview” comes from: which data it reads, how it is queried and how its numbers are worked out.",
      },
    ]);
    expect(conversations.conversations.size).toBe(1);
    const [created] = [...conversations.conversations.values()];
    expect(router.state.location.pathname).toBe(conversationPath(created?.id ?? ""));
    // The way back is this dashboard, the version in the address included.
    const back = await screen.findByRole("link", { name: "Back to Energy overview" });
    expect(back.getAttribute("href")).toBe(`${dashboardPath(dashboard.id)}?version=1`);
    click(back);
    await waitFor(() => expect(router.state.location.pathname).toBe(dashboardPath(dashboard.id)));
    expect(router.state.location.search).toBe("?version=1");
  });

  test("a computation's panel asks about the computation", async () => {
    const { conversations, router } = setUp();
    const card = (await screen.findByRole("heading", { level: 3, name: "Energy this week" })).closest(
      "section",
    ) as HTMLElement;
    // A KPI is its own button: its value opens the computation's source.
    click(within(card).getByRole("button", { name: "Source of Energy this week" }));
    const panel = await within(card).findByRole("region", { name: "Source" });
    expect(within(panel).getByText("sum(daily.kwh)")).toBeTruthy();
    click(within(panel).getByRole("button", { name: "Ask AI to explain" }));
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
    expect(asked(conversations).map((request) => (request.body as { question: string }).question)).toEqual([
      "Explain where “Total energy” on the dashboard “Energy overview” comes from: which data it reads, how it is queried and how its numbers are worked out.",
    ]);
  });
});

describe("device names in a statistics query's summary", () => {
  const dataPoints = (panel: HTMLElement) =>
    within(within(panel).getByTestId("request-summary"))
      .getAllByRole("listitem")
      .map((item) => item.textContent);

  test("a listed device is shown by its name, from one read of the device list", async () => {
    const devices = new FakeDeviceServer();
    const boiler = devices.addDevice({ name: "Boiler B1" });
    const chiller = devices.addDevice({ name: "Chiller C2" });
    setUp({
      devices,
      series: [
        { device_id: boiler.device_id, point: "active_power" },
        { device_id: chiller.device_id, point: "voltage" },
      ],
    });
    const panel = await openSource("Weekly power");
    await waitFor(() => expect(dataPoints(panel)).toEqual(["Boiler B1 · , active_power", "Chiller C2 · , voltage"]));
    // The raw parameters still carry the IDs.
    expect(within(panel).getByRole("group", { name: "The query's raw parameters" }).textContent).toContain(
      boiler.device_id,
    );
    expect(devices.requests.filter((url) => url.pathname === "/api/assets/v1/devices")).toHaveLength(1);
  });

  test("a device the list does not have is shown by its ID", async () => {
    const devices = new FakeDeviceServer();
    const boiler = devices.addDevice({ name: "Boiler B1" });
    setUp({
      devices,
      series: [
        { device_id: boiler.device_id, point: "active_power" },
        { device_id: UNLISTED_DEVICE, point: "voltage" },
      ],
    });
    const panel = await openSource("Weekly power");
    await waitFor(() =>
      expect(dataPoints(panel)).toEqual(["Boiler B1 · , active_power", `${UNLISTED_DEVICE} · , voltage`]),
    );
  });

  test("while the list is loading and when it fails, the ID is shown", async () => {
    const devices = new FakeDeviceServer();
    const boiler = devices.addDevice({ name: "Boiler B1" });
    devices.failNext("devices", fakeError(503, "unavailable", "error.unavailable"));
    setUp({ devices, series: [{ device_id: boiler.device_id, point: "active_power" }] });
    const panel = await openSource("Weekly power");
    expect(dataPoints(panel)).toEqual([`${boiler.device_id} · , active_power`]);
    await until(
      () => devices.requests.some((url) => url.pathname === "/api/assets/v1/devices"),
      "the device list to be requested",
    );
    // The failed lookup is not an error of the panel: the ID stays.
    await act(async () => {});
    expect(dataPoints(panel)).toEqual([`${boiler.device_id} · , active_power`]);
    expect(within(panel).queryByRole("alert")).toBeNull();
  });

  test("the device list is not read until a panel needs a name", async () => {
    const devices = new FakeDeviceServer();
    setUp({ devices });
    await screen.findByRole("heading", { level: 3, name: "Weekly power" });
    const sql = await openSource("Power by line");
    expect(sql).toBeTruthy();
    expect(devices.requests.filter((url) => url.pathname === "/api/assets/v1/devices")).toHaveLength(0);
  });
});
