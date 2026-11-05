import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import type { DeviceHealthRow } from "../../api/pending/home-overview.ts";
import { demoDevices, FakeHomeOverviewServer } from "../../test/fake-home-overview.ts";
import { FakeSetupServer } from "../../test/fake-setup.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { homeRoutes } from "./test-support.tsx";

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

function renderOverview(
  overview: FakeHomeOverviewServer,
  options: { path?: string; role?: "viewer" | "editor" | "admin"; setup?: FakeSetupServer; pages?: string[] } = {},
) {
  return renderAt(options.path ?? "/overview", {
    routes: homeRoutes({
      overview: overview.api(),
      ...(options.setup ? { setup: options.setup.api() } : {}),
      pages: options.pages ?? [],
    }),
    identity: signedInBrowser({ role: options.role ?? "admin" }).browser,
  });
}

async function tiles() {
  const region = await screen.findByRole("region", { name: "Device status" });
  return within(region).getAllByRole("link");
}

describe("the generic home", () => {
  test("shows the status counts, the biggest users, the new anomalies and the device table", async () => {
    renderOverview(new FakeHomeOverviewServer());
    expect(await screen.findByRole("heading", { level: 1, name: "Overview" })).toBeTruthy();

    // Each tile says its state in words and an icon, and names the device when it is the only one.
    const cards = await tiles();
    expect(cards.map((card) => card.textContent)).toEqual([
      "Normal4View normal devices",
      "Abnormal1Boiler B1",
      "Offline1Compressor A2",
    ]);
    expect(cards.map((card) => card.getAttribute("href"))).toEqual([
      "/devices?status=normal",
      "/devices?status=abnormal",
      "/devices?status=offline",
    ]);

    const usage = await screen.findByRole("region", { name: "Most power today" });
    expect(
      within(usage)
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual([
      "Main panel P1412",
      "Compressor A1268",
      "Boiler B1184",
      "Injection press M3151",
      "Cooling tower C1" + "97",
    ]);

    const anomalies = await screen.findByRole("region", { name: "New anomalies since your last visit" });
    const rows = within(anomalies).getAllByRole("listitem");
    expect(rows).toHaveLength(2);
    expect(rows[0]?.textContent).toContain("Boiler B1 · Exhaust temperature above the threshold");
    expect(rows[0]?.textContent).toContain("Abnormal");
    expect(rows[1]?.textContent).toContain("Compressor A2 · No readings");
    expect(rows[1]?.textContent).toContain("Offline");
    expect(rows[1]?.textContent).toContain("still going");

    const table = await screen.findByRole("table");
    const body = within(table).getAllByRole("row").slice(1);
    expect(body).toHaveLength(6);
    // A device opens its page.
    expect(
      within(body[2] as HTMLElement)
        .getByRole("link", { name: "Boiler B1" })
        .getAttribute("href"),
    ).toMatch(/^\/devices\//);
    expect(within(body[2] as HTMLElement).getByText("Abnormal")).toBeTruthy();
    expect(within(body[5] as HTMLElement).getByText("Offline")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Show all/ })).toBeNull();
  });

  test("searching the table narrows it by name, and says so when nothing matches", async () => {
    renderOverview(new FakeHomeOverviewServer());
    const search = await screen.findByRole("searchbox", { name: "Search devices by name" });
    act(() => {
      fireEvent.change(search, { target: { value: "compressor" } });
    });
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(3);
    act(() => {
      fireEvent.change(search, { target: { value: "zzz" } });
    });
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.getByText("No devices match")).toBeTruthy();
  });

  test("a long table shows the first rows and the rest on request", async () => {
    const devices: DeviceHealthRow[] = Array.from({ length: 12 }, (_, index) => ({
      ...(demoDevices()[0] as DeviceHealthRow),
      device_id: `00000000-0000-4000-8000-0000000001${index.toString().padStart(2, "0")}`,
      name: `Meter ${index + 1}`,
    }));
    renderOverview(new FakeHomeOverviewServer(devices));
    const table = await screen.findByRole("table");
    expect(within(table).getAllByRole("row")).toHaveLength(9);
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Show all 12 devices" }));
    });
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(13);
    expect(screen.queryByRole("button", { name: /Show all/ })).toBeNull();
  });

  test("a tenant with no gateway is told the next step: the administrator can start it, a member is told to ask", async () => {
    renderOverview(new FakeHomeOverviewServer([], 0), { pages: ["/gateways"] });
    expect(await screen.findByText("No gateway yet")).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Device status" })).toBeNull();
    expect(screen.getByRole("link", { name: "Bind a gateway" }).getAttribute("href")).toBe("/gateways?bind");
    cleanup();

    // Where the bind page is not a route of this build, the button is left out.
    renderOverview(new FakeHomeOverviewServer([], 0));
    expect(await screen.findByText("No gateway yet")).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Bind a gateway" })).toBeNull();
    cleanup();

    renderOverview(new FakeHomeOverviewServer([], 0), { role: "editor", pages: ["/gateways"] });
    expect(await screen.findByText(/Ask an administrator to bind a gateway/)).toBeTruthy();
    expect(screen.queryByRole("link", { name: /gateway/i })).toBeNull();
  });

  test("devices without a first reading wait for one, with what to check after a while", async () => {
    const devices = demoDevices().map((device) => ({ ...device, last_report_at: null, usage_today_kwh: null }));
    renderOverview(new FakeHomeOverviewServer(devices), { pages: ["/devices"] });
    expect(await screen.findByText("Waiting for the first reading")).toBeTruthy();
    expect(screen.getByText(/6 devices added.*RS-485/)).toBeTruthy();
    expect(screen.getByRole("link", { name: "Check device connections" }).getAttribute("href")).toBe("/devices");
    expect(screen.queryByRole("region", { name: "Device status" })).toBeNull();
  });

  test("a block that fails says so on its own and retries, while the others show", async () => {
    const server = new FakeHomeOverviewServer();
    server.failNext("usage");
    renderOverview(server);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Power usage can't be loaded");
    expect(alert.textContent).toContain("The rest of the page is not affected.");
    // The rest of the page drew.
    expect((await tiles()).length).toBe(3);
    expect(await screen.findByRole("table")).toBeTruthy();
    act(() => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    expect(await screen.findByText("Main panel P1")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  test("when nothing can be read the page says so once, and retrying brings it back", async () => {
    const server = new FakeHomeOverviewServer();
    for (const operation of ["summary", "usage", "anomalies", "devices"] as const) server.failNext(operation);
    renderOverview(server);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Can't reach TrustIoT");
    expect(screen.getAllByRole("alert")).toHaveLength(1);
    act(() => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    expect((await tiles()).length).toBe(3);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  test("the notice that the AI is paused shows only when asked for", async () => {
    renderOverview(new FakeHomeOverviewServer(), { path: "/overview?ai=paused" });
    const title = await screen.findByText("The AI assistant is temporarily unavailable");
    // A notice, not an error: read politely.
    expect(title.closest("[role=status]")).not.toBeNull();
    cleanup();
    renderOverview(new FakeHomeOverviewServer());
    await screen.findByRole("heading", { level: 1, name: "Overview" });
    expect(screen.queryByText("The AI assistant is temporarily unavailable")).toBeNull();
  });

  test("the setup checklist sits above the overview for an administrator", async () => {
    renderOverview(new FakeHomeOverviewServer(), { setup: FakeSetupServer.midway() });
    const region = await screen.findByRole("region", { name: "Get started with TrustIoT" });
    const heading = screen.getByRole("heading", { level: 1, name: "Overview" });
    expect(region.compareDocumentPosition(heading) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  test("refreshing asks every block again", async () => {
    const server = new FakeHomeOverviewServer();
    renderOverview(server);
    await tiles();
    await waitFor(() => expect(server.calls).toHaveLength(4));
    await waitFor(() =>
      expect((screen.getByRole("button", { name: "Refresh" }) as HTMLButtonElement).disabled).toBe(false),
    );
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    });
    await waitFor(() => expect(server.calls).toHaveLength(8));
  });
});
