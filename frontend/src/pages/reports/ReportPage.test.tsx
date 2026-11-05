import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { apiError } from "../../test/fake-identity.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { FakeReportServer } from "../../test/fake-reports.ts";
import { signedInBrowser } from "../../test/render.tsx";
import { recordingSaver, renderReports } from "../../test/render-reports.tsx";
import { formatAmount, formatChange } from "./format.ts";
import { reportPath } from "./paths.ts";

afterEach(() => {
  cleanup();
});

function open(server: FakeReportServer, id: string, role: "viewer" | "editor" | "admin" = "viewer") {
  const saver = recordingSaver();
  const rendered = renderReports(reportPath(id), server, {
    identity: signedInBrowser({ role }).browser,
    conversations: new FakeQuestionServer().api(),
    saver,
  });
  return { ...rendered, saver };
}

function kpi(label: string): HTMLElement {
  return screen.getByText(label).parentElement as HTMLElement;
}

describe("a monthly report", () => {
  function seeded() {
    const server = new FakeReportServer();
    const report = server.addReport("monthly_carbon", { start: "2026-10-01", end: "2026-10-31" });
    return { server, report };
  }

  test("shows the four key figures with what they compare with, from the stored dataset", async () => {
    const { server, report } = seeded();
    open(server, report.id);
    expect(await screen.findByRole("heading", { level: 1, name: "Monthly carbon report October 2026" })).toBeTruthy();
    const { kpis } = report.content ?? { kpis: null };
    if (kpis === null) throw new Error("no content");
    expect(within(kpi("Total energy")).getByText(`${formatAmount(kpis.total_kwh, "en")} kWh`)).toBeTruthy();
    expect(within(kpi("Carbon")).getByText(`${formatAmount(kpis.carbon_kg, "en")} kg`)).toBeTruthy();
    expect(within(kpi("Carbon")).getByText("CO₂e, factor 0.494")).toBeTruthy();
    expect(within(kpi("Change from last month")).getByText(formatChange(0.042, "en"))).toBeTruthy();
    expect(within(kpi("Change from last month")).getByText(/^Last month .* kWh$/)).toBeTruthy();
    expect(within(kpi("Change from last year")).getByText(formatChange(-0.031, "en"))).toBeTruthy();
    expect(screen.getByText(/A report never changes after it is made/)).toBeTruthy();
  });

  test("draws the daily energy as a chart with a one-line summary, and as a table on request", async () => {
    const { server, report } = seeded();
    open(server, report.id);
    const chart = await screen.findByRole("img", { name: /Bar chart of daily energy: the highest is/ });
    expect(chart.querySelectorAll("rect")).toHaveLength(31);
    // Positions are attributes: the CSP forbids style attributes.
    expect(chart.querySelector("[style]")).toBeNull();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "View as a table" }));
    });
    const table = await screen.findByRole("table", { name: "Daily energy in kWh" });
    expect(within(table).getAllByRole("row")).toHaveLength(32);
    expect(screen.queryByRole("img", { name: /Bar chart/ })).toBeNull();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "View as a chart" }));
    });
    expect(await screen.findByRole("img", { name: /Bar chart/ })).toBeTruthy();
  });

  test("lists each device with its energy, carbon and change, and links to its page", async () => {
    const { server, report } = seeded();
    open(server, report.id);
    const table = await screen.findByRole("table", { name: "Energy and carbon of each device" });
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(report.content?.devices.length ?? -1);
    const first = report.content?.devices[0];
    expect(
      within(rows[0] as HTMLElement)
        .getByRole("link", { name: first?.name ?? "" })
        .getAttribute("href"),
    ).toBe(`/devices/${first?.device_id}`);
    expect(within(rows[0] as HTMLElement).getByText(formatAmount(first?.kwh ?? 0, "en"))).toBeTruthy();
  });

  test("any member downloads the PDF and the CSV under names built from the report's type and period", async () => {
    const { server, report } = seeded();
    const { saver } = open(server, report.id, "viewer");
    const pdf = await screen.findByRole("button", { name: /^Download the PDF of/ });
    act(() => {
      fireEvent.click(pdf);
    });
    await waitFor(() => expect(saver.saved).toHaveLength(1));
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: /^Download the CSV of/ }));
    });
    await waitFor(() => expect(saver.saved).toHaveLength(2));
    expect(saver.saved.map((file) => file.name)).toEqual([
      "trustiot-monthly-carbon-2026-10-01.pdf",
      "trustiot-monthly-carbon-2026-10-01.csv",
    ]);
  });

  test("a failed download shows why", async () => {
    const { server, report } = seeded();
    const { saver } = open(server, report.id);
    const csv = await screen.findByRole("button", { name: /^Download the CSV of/ });
    server.failNext("download", apiError(500, "internal", "error.internal"));
    act(() => {
      fireEvent.click(csv);
    });
    expect((await screen.findByText(/Couldn't download the file/)).textContent).toContain("Something went wrong");
    expect(saver.saved).toHaveLength(0);
  });

  test("the questions to the AI start a conversation that names the report and send at once", async () => {
    const { server, report } = seeded();
    const { router } = open(server, report.id);
    const ask = await screen.findByRole("button", { name: "Where can I save energy?" });
    act(() => {
      fireEvent.click(ask);
    });
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\/[0-9a-f-]{36}$/));
    const state = router.state.location.state as { asked: { text: string } };
    expect(state.asked.text).toContain("Where can I save energy?");
    expect(state.asked.text).toContain("Monthly carbon report October 2026");
  });
});

describe("a weekly report", () => {
  test("compares with the week before and has no year-over-year figure", async () => {
    const server = new FakeReportServer();
    const report = server.addReport("weekly_energy", { start: "2026-10-26", end: "2026-11-01" });
    open(server, report.id);
    expect(await screen.findByRole("heading", { level: 1, name: /^Weekly energy report/ })).toBeTruthy();
    expect(within(kpi("Change from last week")).getByText(/^Last week .* kWh$/)).toBeTruthy();
    expect(screen.queryByText("Change from last year")).toBeNull();
  });
});

describe("a report that cannot be shown", () => {
  test("a report that is being made says so", async () => {
    const server = new FakeReportServer();
    const report = server.addReport("weekly_energy", { start: "2026-10-26", end: "2026-11-01" }, "generating");
    open(server, report.id);
    expect(await screen.findByText(/still being made/)).toBeTruthy();
  });

  test("a report that failed says why", async () => {
    const server = new FakeReportServer();
    const report = server.addReport("weekly_energy", { start: "2026-10-26", end: "2026-11-01" }, "failed");
    open(server, report.id);
    expect(await screen.findByText(/could not be made. The query timed out/)).toBeTruthy();
  });

  test("an unknown report is a not-found page with the way back", async () => {
    open(new FakeReportServer(), "00000000-0000-4000-8000-00000000dead");
    expect(await screen.findByRole("heading", { name: "Report not found" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Back to the reports" }).getAttribute("href")).toBe("/reports");
  });

  test("a failed load can be retried", async () => {
    const server = new FakeReportServer();
    const report = server.addReport("monthly_carbon", { start: "2026-10-01", end: "2026-10-31" });
    server.failNext("getReport", apiError(500, "internal", "error.internal"));
    open(server, report.id);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Couldn't load the report");
    act(() => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    expect(await screen.findByRole("heading", { level: 1, name: /Monthly carbon report/ })).toBeTruthy();
  });

  test("a plan without reports gets the wall", async () => {
    const server = new FakeReportServer();
    const report = server.addReport("monthly_carbon", { start: "2026-10-01", end: "2026-10-31" });
    server.plan = "starter";
    open(server, report.id);
    expect(await screen.findByRole("heading", { level: 2, name: /Business plan and above/ })).toBeTruthy();
  });

  test("shows a loading status while the report is on its way", async () => {
    const server = new FakeReportServer();
    const report = server.addReport("monthly_carbon", { start: "2026-10-01", end: "2026-10-31" });
    server.hold("getReport");
    open(server, report.id);
    await waitFor(() =>
      expect(screen.getAllByRole("status").some((node) => node.textContent === "Loading the report…")).toBe(true),
    );
    act(() => server.release());
    expect(await screen.findByRole("heading", { level: 1, name: /Monthly carbon report/ })).toBeTruthy();
  });
});

describe("a report for chosen devices", () => {
  test("lists only those devices, also after a report that was being made is ready", async () => {
    const server = new FakeReportServer();
    server.deviceNames.set("dev-1", "line-2-meter");
    const schedule = server.addSchedule({
      kind: "weekly_energy",
      scope: { kind: "devices", device_ids: ["dev-1"] },
    });
    const made = await server.api().generateNow(schedule.id);
    server.finish();
    open(server, made.id);
    const table = await screen.findByRole("table", { name: "Energy and carbon of each device" });
    expect(within(table).getAllByRole("row").slice(1)).toHaveLength(1);
    expect(within(table).getByRole("link", { name: "line-2-meter" })).toBeTruthy();
    expect(screen.getByText(/^1 chosen devices|^1 devices|chosen devices/)).toBeTruthy();
  });
});
