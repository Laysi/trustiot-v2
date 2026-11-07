import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { DASHBOARDS_PATH, dashboardPath } from "../../app/paths.ts";
import { DAILY_ROWS, SPECS } from "../../dashboard/fixtures.ts";
import { dashboardFormat } from "../../dashboard/format.ts";
import { formatDateTime } from "../../i18n/format.ts";
import { type FakeChart, fakeEngine } from "../../test/fake-chart.ts";
import { FakeDashboardServer, type FakeRole, fakeError } from "../../test/fake-dashboards.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { RunLimiter } from "./useDatasetRuns.ts";

afterEach(() => {
  cleanup();
});

/** The components format as the page does: English, in the user's time zone (UTC in these tests). */
const format = dashboardFormat("en", "UTC");

function open(server: FakeDashboardServer, path: string, options: { role?: FakeRole } = {}) {
  // The server's role decides `permissions`; the session's, which the list reads, is the same.
  if (options.role) server.role = options.role;
  const chart = fakeEngine();
  const rendered = renderAt(path, {
    dashboards: server.api(),
    chartEngine: chart.engine,
    ...(options.role ? { identity: signedInBrowser({ role: options.role }).browser } : {}),
  });
  return { ...rendered, charts: chart.charts };
}

/** The card of the component titled `title`. */
function card(title: string): HTMLElement {
  return screen.getByRole("heading", { level: 3, name: title }).closest("section") as HTMLElement;
}

/** `sum(daily.kwh)`, the fixture KPI's value, as the page shows it. */
function totalOf(rows: readonly (readonly unknown[])[]): string {
  return format.number(rows.reduce((sum, row) => sum + (typeof row[2] === "number" ? row[2] : 0), 0));
}

async function kpiValue(): Promise<string> {
  return (await within(await screen.findByRole("region", { name: "Energy this week" })).findByTestId("value"))
    .textContent as string;
}

async function chartAt(charts: FakeChart[], index = 0): Promise<FakeChart> {
  await waitFor(() => {
    if ((charts[index]?.options.length ?? 0) === 0) throw new Error(`waiting for chart ${index} to get an option`);
  });
  return charts[index] as FakeChart;
}

/** The titles in the list's rows, in order. */
function listedTitles(): string[] {
  return within(screen.getByRole("table"))
    .getAllByRole("rowheader")
    .map((cell) => within(cell).getByRole("link").textContent as string);
}

describe("the dashboard list", () => {
  test("lists the tenant's saved dashboards, newest first, each linking to its page", async () => {
    const server = new FakeDashboardServer();
    const older = server.addDashboard({ title: "Line power", updatedAt: "2026-10-30T08:00:00Z" });
    const newer = server.addDashboard({ title: "Energy overview", updatedAt: "2026-11-01T08:00:00Z" });
    server.addDashboard({ title: "A draft", kind: "draft" });
    open(server, DASHBOARDS_PATH);
    const table = await screen.findByRole("table");
    expect(listedTitles()).toEqual(["Energy overview", "Line power"]);
    const links = within(table).getAllByRole("link", { name: /^Open/ });
    expect(links.map((link) => link.getAttribute("href"))).toEqual([dashboardPath(newer.id), dashboardPath(older.id)]);
    // Who made it and when it was last updated, and the latest version.
    const row = within(table).getByRole("link", { name: "Energy overview" }).closest("tr") as HTMLElement;
    expect(row.textContent).toContain("Another member");
    expect(row.textContent).toContain("Version 1");
    expect(within(row).getByText(formatDateTime("2026-11-01T08:00:00Z", "en", "UTC"))).toBeTruthy();
  });

  test('the "In use" and "Archived" tabs split the dashboards, and the choice is in the address', async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Current" });
    server.addDashboard({ title: "Old plant", archivedAt: "2026-10-31T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z" });
    const { router } = open(server, DASHBOARDS_PATH);
    await screen.findByRole("table");
    expect(listedTitles()).toEqual(["Current"]);
    expect(screen.getByRole("button", { name: "In use" }).getAttribute("aria-pressed")).toBe("true");
    expect(server.requests.at(-1)?.url.searchParams.get("include_archived")).toBeNull();

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Archived" }));
    });
    await waitFor(() => expect(listedTitles()).toEqual(["Old plant"]));
    expect(screen.getByRole("button", { name: "Archived" }).getAttribute("aria-pressed")).toBe("true");
    expect(router.state.location.search).toBe("?tab=archived");
    expect(server.requests.at(-1)?.url.searchParams.get("include_archived")).toBe("true");
    expect(screen.getByText(`Archived ${formatDateTime("2026-10-31T00:00:00Z", "en", "UTC")}`)).toBeTruthy();

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "In use" }));
    });
    await waitFor(() => expect(listedTitles()).toEqual(["Current"]));
    expect(router.state.location.search).toBe("");
  });

  test("a link to the archived tab opens on it", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Old plant", archivedAt: "2026-10-31T00:00:00Z" });
    open(server, `${DASHBOARDS_PATH}?tab=archived`);
    await screen.findByRole("table");
    expect(listedTitles()).toEqual(["Old plant"]);
    expect(screen.getByRole("button", { name: "Archived" }).getAttribute("aria-pressed")).toBe("true");
  });

  test("the search narrows the list by name, and says when nothing matches", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Cooling tower", updatedAt: "2026-11-01T00:00:00Z" });
    server.addDashboard({ title: "Air compressor", updatedAt: "2026-10-01T00:00:00Z" });
    open(server, DASHBOARDS_PATH);
    await screen.findByRole("table");
    const search = screen.getByRole("searchbox", { name: "Search dashboards" });
    act(() => {
      fireEvent.change(search, { target: { value: "cooling" } });
    });
    expect(listedTitles()).toEqual(["Cooling tower"]);
    act(() => {
      fireEvent.change(search, { target: { value: "boiler" } });
    });
    expect(await screen.findByText('No dashboard matches "boiler"')).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
  });

  test("with no dashboards it says how to make one, in each tab", async () => {
    open(new FakeDashboardServer(), DASHBOARDS_PATH);
    expect(await screen.findByText("No saved dashboards yet")).toBeTruthy();
    expect(screen.getByText(/Ask TrustIoT for a dashboard in a conversation/)).toBeTruthy();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Archived" }));
    });
    expect(await screen.findByText("No archived dashboards")).toBeTruthy();
  });

  test("while it loads one status says so; a failure says why and retries", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Energy overview" });
    server.failNext("list", fakeError(503, "unavailable", "error.unavailable"));
    server.hold(["list"]);
    open(server, DASHBOARDS_PATH);
    expect(await screen.findByText("Loading dashboards…")).toBeTruthy();
    act(() => server.release());
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Couldn't load the dashboards");
    act(() => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    await screen.findByRole("table");
    expect(listedTitles()).toEqual(["Energy overview"]);
  });

  test("more than a page loads the next one on request", async () => {
    const server = new FakeDashboardServer();
    server.pageSize = 2;
    for (const [index, title] of ["One", "Two", "Three"].entries()) {
      server.addDashboard({ title, updatedAt: `2026-11-0${3 - index}T00:00:00Z` });
    }
    open(server, DASHBOARDS_PATH);
    await screen.findByRole("table");
    expect(screen.getByText("2 shown so far · newest update first")).toBeTruthy();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    });
    await waitFor(() => expect(listedTitles()).toEqual(["One", "Two", "Three"]));
    expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
  });

  test("the menu leads to the list, and the list to a dashboard", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ title: "Energy overview" });
    const { router } = open(server, "/");
    const trigger = await screen.findByRole("button", { name: "Menu" });
    await act(async () => {
      fireEvent.keyDown(trigger, { key: "Enter" });
    });
    act(() => {
      fireEvent.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: "Saved dashboards" }));
    });
    const table = await screen.findByRole("table");
    expect(router.state.location.pathname).toBe(DASHBOARDS_PATH);
    act(() => {
      fireEvent.click(within(table).getByRole("link", { name: "Energy overview" }));
    });
    expect(await screen.findByRole("heading", { level: 1, name: "Energy overview" })).toBeTruthy();
    expect(router.state.location.pathname).toBe(dashboardPath(dashboard.id));
  });

  test("an editor archives and unarchives at once, and the status line says what happened", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ title: "Energy overview" });
    server.addDashboard({ title: "Line power", updatedAt: "2020-01-01T00:00:00Z" });
    open(server, DASHBOARDS_PATH);
    await screen.findByRole("table");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: 'Archive "Energy overview"' }));
    });
    // No dialog: it takes effect at once.
    expect(await screen.findByText('Archived "Energy overview". You can unarchive it under "Archived".')).toBeTruthy();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(dashboard.archived_at).toBeDefined();
    await waitFor(() => expect(listedTitles()).toEqual(["Line power"]));

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Archived" }));
    });
    await waitFor(() => expect(listedTitles()).toEqual(["Energy overview"]));
    expect(screen.queryByText(/^Archived "Energy/)).toBeNull();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: 'Unarchive "Energy overview"' }));
    });
    expect(await screen.findByText('Unarchived "Energy overview": it is in use again.')).toBeTruthy();
    expect(dashboard.archived_at).toBeUndefined();
  });

  test("a viewer sees the dashboards and a note, and no archive or unarchive", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Energy overview" });
    server.addDashboard({ title: "Old plant", archivedAt: "2026-10-31T00:00:00Z" });
    open(server, DASHBOARDS_PATH, { role: "viewer" });
    await screen.findByRole("table");
    expect(screen.getByText("You're a viewer")).toBeTruthy();
    expect(screen.getByRole("link", { name: 'Open "Energy overview"' })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^Archive "/ })).toBeNull();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Archived" }));
    });
    await waitFor(() => expect(listedTitles()).toEqual(["Old plant"]));
    expect(screen.queryByRole("button", { name: /^Unarchive/ })).toBeNull();
  });

  test("an archive the server refuses says why in the status line", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Energy overview" });
    open(server, DASHBOARDS_PATH);
    await screen.findByRole("table");
    server.failNext("archive", fakeError(403, "permission_denied", "dashboard.editor_required"));
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: 'Archive "Energy overview"' }));
    });
    expect(await screen.findByText("Only editors and admins can publish, restore or archive dashboards.")).toBeTruthy();
    expect(listedTitles()).toEqual(["Energy overview"]);
  });
});

describe("a dashboard's page", () => {
  test("a deep link opens the dashboard and version it names, and a reload shows the same", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.timeSeries, SPECS.kpi]] });
    const path = dashboardPath(dashboard.id, 1);
    const first = open(server, path);
    expect(await screen.findByText("Version 1 (latest is 2)")).toBeTruthy();
    expect(await screen.findByRole("heading", { level: 3, name: "Meters" })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 3, name: "Power by line" })).toBeNull();
    const reloaded = `${first.router.state.location.pathname}${first.router.state.location.search}`;
    expect(reloaded).toBe(path);
    cleanup();

    open(server, reloaded);
    expect(await screen.findByText("Version 1 (latest is 2)")).toBeTruthy();
    expect(await screen.findByRole("heading", { level: 3, name: "Meters" })).toBeTruthy();
    expect(
      server.requests
        .filter((request) => request.method === "GET")
        .at(-1)
        ?.url.searchParams.get("version"),
    ).toBe("1");
  });

  test("without a version it shows the latest; an unknown dashboard or version says so", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id));
    expect(await screen.findByText("Published · version 2")).toBeTruthy();
    cleanup();

    open(server, dashboardPath(dashboard.id, 9));
    expect(await screen.findByRole("heading", { level: 1, name: "Version not found" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Show the latest version" }).getAttribute("href")).toBe(
      dashboardPath(dashboard.id),
    );
    cleanup();

    open(server, dashboardPath("00000000-0000-4000-8000-00000000ffff"));
    expect(await screen.findByRole("heading", { level: 1, name: "Dashboard not found" })).toBeTruthy();
    cleanup();

    const before = server.requests.length;
    open(server, "/dashboards/not-a-uuid");
    expect(await screen.findByRole("heading", { level: 1, name: "Dashboard not found" })).toBeTruthy();
    expect(server.requests.length).toBe(before);
  });

  test("opening runs every dataset of the version as the viewer, the shown components' first, two at a time", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.timeSeries, SPECS.kpi]] });
    server.hold();
    open(server, dashboardPath(dashboard.id));
    await screen.findByRole("heading", { level: 1, name: "Energy overview" });
    await waitFor(() => expect(server.runs()).toHaveLength(2));
    // Two run at once; the others wait for a slot.
    expect(server.running).toBe(2);
    expect(within(card("Energy this week")).getByRole("status").textContent).toContain("Loading data");
    act(() => server.release());
    await waitFor(() => expect(server.runs()).toHaveLength(4));
    expect(server.runs()).toEqual([
      { version: 1, dataset: "power" },
      { version: 1, dataset: "daily" },
      { version: 1, dataset: "trend" },
      { version: 1, dataset: "readings" },
    ]);
    expect(server.maxRunning).toBe(2);
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
    // The items are drawn in the order of the component list.
    expect(screen.getAllByRole("heading", { level: 3 }).map((heading) => heading.textContent)).toEqual([
      "Power by line",
      "Energy this week",
    ]);
  });

  test("the first load is skeletons, announced once; the page says when the data was queried and what time it covers", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.timeSeries, SPECS.kpi]] });
    server.hold(["dashboard"]);
    open(server, dashboardPath(dashboard.id));
    const status = await screen.findByText("Loading the dashboard…");
    expect(status.getAttribute("role")).toBe("status");
    expect(document.querySelector('[aria-busy="true"]')).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 1 })).toBeNull();
    act(() => server.release());
    await screen.findByRole("heading", { level: 1, name: "Energy overview" });
    await kpiValue();
    const queriedAt = server.results.get("daily")?.meta.queried_at ?? "";
    expect(screen.getByText(`Data time ${formatDateTime(queriedAt, "en", "UTC")}`)).toBeTruthy();
    expect(screen.getByText("Queried again at every opening")).toBeTruthy();
    // The actual first and last time of the results (the fixtures' time columns).
    expect(
      screen.getByText(
        `Data from ${format.wallTime(Date.UTC(2026, 9, 26))} to ${format.wallTime(Date.UTC(2026, 10, 1, 1, 6, 7, 250))}`,
      ),
    ).toBeTruthy();
    // The breadcrumbs lead back to the list, the version is a tag, and "Refresh data" is there.
    const trail = screen.getByRole("navigation", { name: "Breadcrumbs" });
    expect(within(trail).getByRole("link", { name: "Saved dashboards" }).getAttribute("href")).toBe(DASHBOARDS_PATH);
    expect(within(trail).getByText("Energy overview").getAttribute("aria-current")).toBe("page");
    expect(screen.getByText("Published · version 1")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Refresh data" })).toBeTruthy();
  });

  test("refresh runs every dataset again and keeps the earlier content, dimmed, until the new data replaces it", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.timeSeries, SPECS.kpi]] });
    const { charts } = open(server, dashboardPath(dashboard.id));
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
    const chart = await chartAt(charts);
    expect(card("Power by line").contains(chart.root)).toBe(true);
    const items = screen.getByRole("region", { name: "Dashboard contents" });
    expect(items.className).not.toContain("opacity-60");
    expect(items.getAttribute("aria-busy")).toBe("false");
    const firstQueried = server.results.get("daily")?.meta.queried_at ?? "";

    const newRows = [...DAILY_ROWS, ["Thu", "Site 1", 77.125]];
    server.setResult("daily", newRows, { queriedAt: "2026-11-02T03:00:00Z" });
    server.setResult("power", [["2026-11-01 01:00:00", "Line A", 99.5]], { queriedAt: "2026-11-02T03:00:00Z" });
    server.hold();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Refresh data" }));
    });
    // While the new runs are on their way the earlier content stays, dimmed, and the page says so.
    await waitFor(() => expect(screen.getByText("Updating…")).toBeTruthy());
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
    expect(items.className).toContain("opacity-60");
    expect(items.getAttribute("aria-busy")).toBe("true");
    expect(screen.queryByText("Loading data")).toBeNull();
    expect(screen.getByText(`Data time ${formatDateTime(firstQueried, "en", "UTC")}`)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Refresh data" }) as HTMLButtonElement).disabled).toBe(true);
    act(() => server.release());

    expect(await screen.findByText(totalOf(newRows))).toBeTruthy();
    await waitFor(() => expect(screen.queryByText("Updating…")).toBeNull());
    expect(items.className).not.toContain("opacity-60");
    // Every dataset was asked for again: nothing was reused.
    await waitFor(() => expect(server.runs()).toHaveLength(8));
    expect(server.runs().slice(4)).toEqual(server.runs().slice(0, 4));
    // The time series' chart was given the new rows.
    await waitFor(() => {
      const drawn = charts.filter((candidate) => !candidate.disposed && card("Power by line").contains(candidate.root));
      expect(JSON.stringify(drawn.at(-1)?.option.series)).toContain("99.5");
    });
    // The data time is the new one, and each card says when its data was queried.
    expect(screen.getByText(`Data time ${formatDateTime("2026-11-02T03:00:00Z", "en", "UTC")}`)).toBeTruthy();
    expect(within(card("Energy this week")).getByTestId("card-facts").textContent).toContain(
      `Queried at ${format.instant("2026-11-02T03:00:00Z")}`,
    );
  });

  test("a refresh that fails shows the failure for the dataset that failed, not the earlier result", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.timeSeries, SPECS.kpi]] });
    const { charts } = open(server, dashboardPath(dashboard.id));
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
    await chartAt(charts);
    server.failNext("run:daily", fakeError(504, "timeout", "dashboard.dataset_timeout"));
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Refresh data" }));
    });
    await waitFor(() =>
      expect(within(card("Energy this week")).getByTestId("problem").textContent).toContain(
        "The query took too long. Try again later.",
      ),
    );
    // The other datasets were replaced as usual.
    expect(within(card("Power by line")).queryByTestId("problem")).toBeNull();
  });

  test("a value's source shows the formula, the datasets' queries and when they ran, as the API returned them", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id));
    await kpiValue();
    act(() => {
      fireEvent.click(within(card("Energy this week")).getByTestId("value").closest("button") as HTMLElement);
    });
    const panel = await screen.findByRole("region", { name: "Source" });
    const daily = server.results.get("daily");
    expect(within(panel).getByText("sum(daily.kwh)")).toBeTruthy();
    const view = dashboard.versions[0]?.spec.datasets.find((dataset) => dataset.name === "daily");
    expect(view?.source.kind).toBe("sql");
    expect(within(panel).getByText(view?.source.kind === "sql" ? view.source.sql : "")).toBeTruthy();
    expect(within(panel).getByText(format.instant(daily?.meta.queried_at ?? ""))).toBeTruthy();
  });

  test("a chart point's source lists its rows and the query, and the card's header opens the whole dataset", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.timeSeries]] });
    server.setResult("power", [["2026-11-01 00:00:00", "Line A", 12.5]], { queriedAt: "2026-11-02T01:15:00Z" });
    const { charts } = open(server, dashboardPath(dashboard.id));
    const chart = await chartAt(charts);
    act(() => chart.showTip(0, 0));
    act(() => chart.emitSurface("click"));
    const panel = await screen.findByRole("region", { name: "Source" });
    expect(within(panel).getByText("SELECT ts, line, kw FROM power ORDER BY ts")).toBeTruthy();
    expect(within(panel).getByText(format.instant("2026-11-02T01:15:00Z"))).toBeTruthy();
    expect(
      within(panel)
        .getAllByTestId("source-cell")
        .map((cell) => cell.textContent),
    ).toEqual([format.cell("time", "2026-11-01 00:00:00"), "Line A", format.number(12.5)]);

    act(() => {
      fireEvent.click(within(panel).getByRole("button", { name: "Close the source" }));
    });
    act(() => {
      fireEvent.click(within(card("Power by line")).getByRole("button", { name: "Source of Power by line" }));
    });
    const whole = await screen.findByRole("region", { name: "Source" });
    expect(within(whole).getByText("Rows").nextElementSibling?.textContent).toBe("1");
    expect(within(whole).queryAllByTestId("source-cell")).toHaveLength(0);
  });

  test("a failed dataset affects only the components that use it, says why and runs again", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.timeSeries, SPECS.kpi]] });
    server.failNext("run:daily", fakeError(504, "timeout", "dashboard.dataset_timeout"));
    const { charts } = open(server, dashboardPath(dashboard.id));
    const kpi = await waitFor(() => {
      const problem = within(card("Energy this week")).getByTestId("problem");
      return problem;
    });
    expect(kpi.textContent).toContain("The query took too long. Try again later.");
    // The time series reads another dataset and is drawn.
    await chartAt(charts);
    expect(within(card("Power by line")).queryByTestId("problem")).toBeNull();

    const before = server.runs().length;
    act(() => {
      fireEvent.click(within(card("Energy this week")).getByRole("button", { name: "Run again" }));
    });
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
    expect(server.runs().slice(before)).toEqual([{ version: 1, dataset: "daily" }]);
  });

  test("a 429 says to try again shortly", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.kpi]] });
    server.failNext(
      "run:daily",
      new Response(
        JSON.stringify({ code: "rate_limited", message_key: "dashboard.runs_limited", details: { retry_after: 3 } }),
        {
          status: 429,
          headers: { "content-type": "application/json", "retry-after": "3" },
        },
      ),
    );
    open(server, dashboardPath(dashboard.id));
    await waitFor(() =>
      expect(within(card("Energy this week")).getByTestId("problem").textContent).toContain(
        "Dashboards in your organization are loading too much data right now. Try again shortly.",
      ),
    );
    // Not retried by itself: the user decides when.
    expect(server.runs().filter((run) => run.dataset === "daily")).toHaveLength(1);
  });

  test("a custom page item shows the sandbox frame in its place, with the card the frame draws", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({
      title: "Plant overview",
      versions: [[SPECS.kpi, { kind: "custom_page" }]],
      page: '<div id="chart"></div><script>tq.ready()</script>',
    });
    open(server, dashboardPath(dashboard.id));
    const frame = await waitFor(() => {
      const found = document.querySelector("iframe");
      if (found === null) throw new Error("waiting for the sandbox frame");
      return found;
    });
    // The sandbox's own document at the runtime's version, in an iframe that can do nothing a
    // sandbox does not grant (the allow-scripts token only; no same-origin).
    expect(frame.getAttribute("src")).toMatch(/^\/sandbox\/v1\/sandbox\.html(#|\?|$)/);
    const sandbox = frame.getAttribute("sandbox") ?? "";
    expect(sandbox.split(/\s+/)).toContain("allow-scripts");
    expect(sandbox.split(/\s+/)).not.toContain("allow-same-origin");
    // The frame's card: its heading and accessible name are the version's title, with the label
    // saying the page is drawn by AI-written code. It is the only card for the item.
    const custom = document.querySelector('[data-component="custom_page"]') as HTMLElement;
    expect(within(custom).getByRole("heading", { level: 3, name: "Plant overview" })).toBeTruthy();
    expect(within(custom).getByText("Custom chart")).toBeTruthy();
    expect(frame.getAttribute("title")).toBe("Plant overview");
    expect(custom.contains(frame)).toBe(true);
    expect(screen.queryByText("This item can't be shown.")).toBeNull();
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
    // The page keeps its place in the list and takes a whole row, like a chart.
    expect(
      [...document.querySelectorAll("[data-component]")].map((element) => element.getAttribute("data-component")),
    ).toEqual(["kpi", "custom_page"]);
    expect(custom.parentElement?.className).toContain("sm:col-span-2");
  });

  test("a custom page item whose version has no page says it can't be shown, and loads no frame", async () => {
    const server = new FakeDashboardServer();
    // The API never gives this (a version with a custom_page item has its page and runtime); the
    // fixture leaves them out to reach the page's fallback.
    const dashboard = server.addDashboard({ versions: [[SPECS.kpi, { kind: "custom_page" }]] });
    open(server, dashboardPath(dashboard.id));
    expect(await screen.findByText("This item can't be shown.")).toBeTruthy();
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
    expect(document.querySelector("iframe")).toBeNull();
    expect(
      [...document.querySelectorAll("[data-component]")].map((element) => element.getAttribute("data-component")),
    ).toEqual(["kpi", "custom_page"]);
  });

  test("a version written before the component list shows its page in the frame, and an unknown item says so", async () => {
    const server = new FakeDashboardServer();
    const old = server.addDashboard({ versions: [[]] });
    const version = old.versions[0];
    if (version === undefined) throw new Error("no version");
    version.page = "<p>old page</p>";
    version.runtime = 1;
    delete version.spec.components;
    open(server, dashboardPath(old.id));
    const frame = await waitFor(() => {
      const found = document.querySelector("iframe");
      if (found === null) throw new Error("waiting for the sandbox frame");
      return found;
    });
    expect(frame.getAttribute("src")).toContain("/sandbox/v1/sandbox.html");
    cleanup();

    // Each item is parsed on its own: an unknown kind, or a known kind with a field this build does
    // not know (a color here), is refused, and the other items are still drawn.
    const newer = server.addDashboard({
      versions: [[{ kind: "gauge", title: "Pressure" }, { ...SPECS.kpi, color: "red" }, SPECS.kpi]],
    });
    open(server, dashboardPath(newer.id));
    expect(await screen.findAllByRole("heading", { level: 3, name: "Item can't be shown" })).toHaveLength(2);
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
  });
});

describe("versions, restore and archive", () => {
  async function openVersions() {
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Version history" }));
    });
    return screen.findByRole("region", { name: "Version history" });
  }

  test("the version history lists each version; viewing an older one shows its components and datasets and says it is older", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.timeSeries, SPECS.kpi]] });
    const { router } = open(server, dashboardPath(dashboard.id));
    await screen.findByText("Published · version 2");
    const panel = await openVersions();
    const items = await within(panel).findAllByRole("listitem");
    expect(items.map((item) => item.getAttribute("aria-current"))).toEqual(["true", null]);
    expect(within(items[0] as HTMLElement).getByText("Latest")).toBeTruthy();
    expect(within(items[0] as HTMLElement).getByText("Shown")).toBeTruthy();
    expect(items[1]?.textContent).toContain("Published");
    act(() => {
      fireEvent.click(within(panel).getByRole("link", { name: "View version 1" }));
    });
    // The tag and the notice both say it is not the latest.
    expect(await screen.findByText("Version 1 (latest is 2)")).toBeTruthy();
    expect(screen.getByText("You're viewing version 1. The latest is version 2.")).toBeTruthy();
    expect(router.state.location.search).toBe("?version=1");
    expect(await screen.findByRole("heading", { level: 3, name: "Meters" })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 3, name: "Power by line" })).toBeNull();
    // Its datasets ran under version 1.
    await waitFor(() => expect(server.runs().filter((run) => run.version === 1)).toHaveLength(4));
  });

  test("restoring an older version from its page asks first, then shows the latest, which has the older one's content", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.timeSeries, SPECS.kpi]] });
    const { router } = open(server, dashboardPath(dashboard.id, 1));
    await screen.findByText("Version 1 (latest is 2)");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Restore version 1" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    expect(document.activeElement?.textContent).toBe("Cancel");
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Restore version 1" }));
    });
    expect(await screen.findByText("Published · version 3")).toBeTruthy();
    expect(screen.getByText("Restored version 1 as version 3.")).toBeTruthy();
    expect(router.state.location.search).toBe("");
    const restore = server.requests.find((request) => request.url.pathname.endsWith("/restore"));
    expect(restore?.body).toEqual({ from_version: 1, base_version: 2 });
    // Version 3 has version 1's components, and its datasets run under version 3.
    expect(await screen.findByRole("heading", { level: 3, name: "Meters" })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 3, name: "Power by line" })).toBeNull();
    await waitFor(() => expect(server.runs().filter((run) => run.version === 3)).toHaveLength(4));
  });

  test("an editor restores from a row of the version history; the latest version has no restore", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id));
    await screen.findByText("Published · version 2");
    const panel = await openVersions();
    await within(panel).findAllByRole("listitem");
    expect(within(panel).getAllByRole("button", { name: /^Restore version/ })).toHaveLength(1);
    act(() => {
      fireEvent.click(within(panel).getByRole("button", { name: "Restore version 1" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Restore version 1" }));
    });
    expect(await screen.findByText("Published · version 3")).toBeTruthy();
    expect(screen.getByText("Restored version 1 as version 3.")).toBeTruthy();
    // The history is read again and has the new version, the latest, on top.
    const again = screen.getByRole("region", { name: "Version history" });
    await waitFor(() => expect(within(again).getAllByRole("listitem")).toHaveLength(3));
    expect(within(again).getAllByRole("listitem")[0]?.textContent).toContain("Restored from version 1");
  });

  test("a viewer sees neither restore nor archive and is told the page is read-only; an editor sees both", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id, 1), { role: "viewer" });
    expect(await screen.findByText("Version 1 (latest is 2)")).toBeTruthy();
    expect(screen.getByText("You're a viewer, so this dashboard is read-only")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Show the latest version" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Refresh data" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^Restore/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Archive" })).toBeNull();
    const panel = await openVersions();
    await within(panel).findAllByRole("listitem");
    expect(within(panel).queryByRole("button", { name: /^Restore/ })).toBeNull();
    expect(within(panel).getByRole("link", { name: "View version 2" })).toBeTruthy();
    cleanup();

    open(server, dashboardPath(dashboard.id, 1), { role: "editor" });
    expect(await screen.findByText("Version 1 (latest is 2)")).toBeTruthy();
    expect(screen.queryByText("You're a viewer, so this dashboard is read-only")).toBeNull();
    expect(screen.getByRole("button", { name: "Restore version 1" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Archive" })).toBeTruthy();
  });

  test("a refused restore says why in the dialog, translated", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id, 1));
    await screen.findByText("Version 1 (latest is 2)");
    // Someone else restored a version in the meantime.
    server.failNext("restore", fakeError(409, "conflict", "dashboard.version_conflict", { latest: 3 }));
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Restore version 1" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Restore version 1" }));
    });
    expect(
      await within(dialog).findByText(
        "The dashboard changed while you were looking at it (it is now at version 3). Reload it and try again.",
      ),
    ).toBeTruthy();

    server.failNext("restore", fakeError(409, "conflict", "dashboard.restore_unchanged"));
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Restore version 1" }));
    });
    expect(
      await within(dialog).findByText(
        "This version is the same as the dashboard's latest version, so restoring it would change nothing.",
      ),
    ).toBeTruthy();

    // The limits on versions are said in words, too, and the dialog stays open throughout.
    server.failNext("restore", fakeError(429, "rate_limited", "dashboard.versions_limited"));
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Restore version 1" }));
    });
    expect(
      await within(dialog).findByText(/published or restored many dashboard versions in the last minute/),
    ).toBeTruthy();
    server.failNext("restore", fakeError(409, "conflict", "dashboard.too_many_versions"));
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Restore version 1" }));
    });
    expect(await within(dialog).findByText(/has 10,000 versions, the most it can have/)).toBeTruthy();
    expect(screen.getByRole("alertdialog")).toBe(dialog);
  });

  test("archiving takes effect at once, says so, and unarchiving undoes it; an archived dashboard has no restore", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id));
    await screen.findByText("Published · version 2");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Archive" }));
    });
    // No dialog; the status says what happened, and the dashboard shows it is archived.
    expect(await screen.findByText('Archived "Energy overview". You can unarchive it under "Archived".')).toBeTruthy();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(
      await screen.findByText(
        "This dashboard is archived: it isn't in the list of dashboards in use and gets no new versions.",
      ),
    ).toBeTruthy();
    expect(dashboard.archived_at).toBeDefined();
    // Archiving did not run the datasets again: it is not a refresh.
    expect(server.runs()).toHaveLength(4);
    const panel = await openVersions();
    await within(panel).findAllByRole("listitem");
    expect(within(panel).queryByRole("button", { name: /^Restore/ })).toBeNull();

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Unarchive" }));
    });
    await waitFor(() => expect(screen.queryByText(/This dashboard is archived/)).toBeNull());
    expect(screen.getByText('Unarchived "Energy overview": it is in use again.')).toBeTruthy();
    expect(
      server.requests.filter((request) => request.url.pathname.endsWith("/archive")).map((request) => request.method),
    ).toEqual(["PUT", "DELETE"]);
    expect(dashboard.archived_at).toBeUndefined();
  });

  test("an archive refused by the server says why", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id));
    await screen.findByText("Published · version 1");
    server.failNext("archive", fakeError(403, "permission_denied", "dashboard.editor_required"));
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Archive" }));
    });
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Only editors and admins can publish, restore or archive dashboards.");
  });
});

describe("RunLimiter", () => {
  test("runs at most its limit at once, in order, and drops a task aborted while it waits", async () => {
    const limiter = new RunLimiter(2);
    const started: number[] = [];
    const finish: (() => void)[] = [];
    const task = (id: number) => () =>
      new Promise<number>((resolve) => {
        started.push(id);
        finish.push(() => resolve(id));
      });
    const aborted = new AbortController();
    const runs = [
      limiter.run(task(1), new AbortController().signal),
      limiter.run(task(2), new AbortController().signal),
      limiter.run(task(3), aborted.signal),
      limiter.run(task(4), new AbortController().signal),
    ];
    expect(started).toEqual([1, 2]);
    aborted.abort(new Error("cancelled"));
    expect(runs[2]).rejects.toThrow("cancelled");
    finish[0]?.();
    await runs[0];
    await waitFor(() => expect(started).toEqual([1, 2, 4]));
    finish[1]?.();
    finish[2]?.();
    expect(await Promise.all([runs[1], runs[3]])).toEqual([2, 4]);
  });

  test("a task that throws synchronously gives its slot back, so the next task starts", async () => {
    const limiter = new RunLimiter(1);
    const started: string[] = [];
    const broken = limiter.run(() => {
      throw new Error("boom");
    }, new AbortController().signal);
    const next = limiter.run(async () => {
      started.push("next");
      return 2;
    }, new AbortController().signal);
    await expect(broken).rejects.toThrow("boom");
    expect(await next).toBe(2);
    expect(started).toEqual(["next"]);
  });
});
