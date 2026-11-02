import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { DASHBOARDS_PATH, dashboardPath } from "../../app/paths.ts";
import { DAILY_ROWS, SPECS } from "../../dashboard/fixtures.ts";
import { dashboardFormat } from "../../dashboard/format.ts";
import { type FakeChart, fakeEngine } from "../../test/fake-chart.ts";
import { FakeDashboardServer, type FakeRole, fakeError } from "../../test/fake-dashboards.ts";
import { renderAt } from "../../test/render.tsx";
import { RunLimiter } from "./useDatasetRuns.ts";

afterEach(() => {
  cleanup();
});

/** The components format as the page does: English, in the user's time zone (UTC in these tests). */
const format = dashboardFormat("en", "UTC");

function open(server: FakeDashboardServer, path: string, options: { role?: FakeRole } = {}) {
  if (options.role) server.role = options.role;
  const chart = fakeEngine();
  const rendered = renderAt(path, { dashboards: server.api(), chartEngine: chart.engine });
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

describe("the dashboard list", () => {
  test("lists the tenant's saved dashboards, newest first, each linking to its page", async () => {
    const server = new FakeDashboardServer();
    const older = server.addDashboard({ title: "Line power", updatedAt: "2026-10-30T08:00:00Z" });
    const newer = server.addDashboard({ title: "Energy overview", updatedAt: "2026-11-01T08:00:00Z" });
    server.addDashboard({ title: "A draft", kind: "draft" });
    open(server, DASHBOARDS_PATH);
    const table = await screen.findByRole("table");
    const links = within(table).getAllByRole("link");
    expect(links.map((link) => link.textContent)).toEqual(["Energy overview", "Line power"]);
    expect(links.map((link) => link.getAttribute("href"))).toEqual([dashboardPath(newer.id), dashboardPath(older.id)]);
  });

  test("archived dashboards show only when asked for, and the choice is in the address", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Current" });
    server.addDashboard({ title: "Old plant", archivedAt: "2026-10-31T00:00:00Z", updatedAt: "2026-10-01T00:00:00Z" });
    const { router } = open(server, DASHBOARDS_PATH);
    const table = await screen.findByRole("table");
    expect(
      within(table)
        .getAllByRole("link")
        .map((link) => link.textContent),
    ).toEqual(["Current"]);

    act(() => {
      fireEvent.click(screen.getByRole("checkbox", { name: "Show archived dashboards" }));
    });
    await waitFor(() => expect(within(screen.getByRole("table")).getAllByRole("link")).toHaveLength(2));
    const archivedRow = within(screen.getByRole("table")).getByRole("link", { name: "Old plant" }).closest("tr");
    expect(within(archivedRow as HTMLElement).getByText("Archived")).toBeTruthy();
    expect(router.state.location.search).toBe("?archived=1");
    expect(server.requests.at(-1)?.url.searchParams.get("include_archived")).toBe("true");
  });

  test("a link with archived ones asked for opens with them", async () => {
    const server = new FakeDashboardServer();
    server.addDashboard({ title: "Old plant", archivedAt: "2026-10-31T00:00:00Z" });
    open(server, `${DASHBOARDS_PATH}?archived=1`);
    expect(await within(await screen.findByRole("table")).findByRole("link", { name: "Old plant" })).toBeTruthy();
    expect((screen.getByRole("checkbox", { name: "Show archived dashboards" }) as HTMLInputElement).checked).toBe(true);
  });

  test("with no dashboards it says how to make one", async () => {
    open(new FakeDashboardServer(), DASHBOARDS_PATH);
    expect(await screen.findByText("No dashboards yet")).toBeTruthy();
    expect(screen.getByText(/Ask the assistant to make a dashboard in a conversation/)).toBeTruthy();
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
    expect(await within(await screen.findByRole("table")).findByRole("link", { name: "Energy overview" })).toBeTruthy();
  });

  test("more than a page loads the next one on request", async () => {
    const server = new FakeDashboardServer();
    server.pageSize = 2;
    for (const [index, title] of ["One", "Two", "Three"].entries()) {
      server.addDashboard({ title, updatedAt: `2026-11-0${3 - index}T00:00:00Z` });
    }
    open(server, DASHBOARDS_PATH);
    await screen.findByRole("table");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Show more" }));
    });
    await waitFor(() =>
      expect(
        within(screen.getByRole("table"))
          .getAllByRole("link")
          .map((link) => link.textContent),
      ).toEqual(["One", "Two", "Three"]),
    );
    expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
  });

  test("the main navigation leads to the list, and the list to a dashboard", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ title: "Energy overview" });
    const { router } = open(server, "/");
    const navigation = await screen.findByRole("navigation", { name: "Main navigation" });
    act(() => {
      fireEvent.click(within(navigation).getByRole("link", { name: "Dashboards" }));
    });
    const table = await screen.findByRole("table");
    act(() => {
      fireEvent.click(within(table).getByRole("link", { name: "Energy overview" }));
    });
    expect(await screen.findByRole("heading", { level: 1, name: "Energy overview" })).toBeTruthy();
    expect(router.state.location.pathname).toBe(dashboardPath(dashboard.id));
  });
});

describe("a dashboard's page", () => {
  test("a deep link opens the dashboard and version it names, and a reload shows the same", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.timeSeries, SPECS.kpi]] });
    const path = dashboardPath(dashboard.id, 1);
    const first = open(server, path);
    expect(await screen.findByText("Version 1 of 2")).toBeTruthy();
    expect(await screen.findByRole("heading", { level: 3, name: "Meters" })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 3, name: "Power by line" })).toBeNull();
    const reloaded = `${first.router.state.location.pathname}${first.router.state.location.search}`;
    expect(reloaded).toBe(path);
    cleanup();

    open(server, reloaded);
    expect(await screen.findByText("Version 1 of 2")).toBeTruthy();
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
    expect(await screen.findByText("Version 2 (latest)")).toBeTruthy();
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

  test("refresh runs every dataset again and shows the new data, not the earlier results", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.timeSeries, SPECS.kpi]] });
    const { charts } = open(server, dashboardPath(dashboard.id));
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
    const chart = await chartAt(charts);
    expect(card("Power by line").contains(chart.root)).toBe(true);

    const newRows = [...DAILY_ROWS, ["Thu", "Site 1", 77.125]];
    server.setResult("daily", newRows, { queriedAt: "2026-11-02T03:00:00Z" });
    server.setResult("power", [["2026-11-01 01:00:00", "Line A", 99.5]], { queriedAt: "2026-11-02T03:00:00Z" });
    server.hold();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    });
    // The earlier results are dropped while the new runs are on their way.
    await waitFor(() =>
      expect(within(card("Energy this week")).getByRole("status").textContent).toContain("Loading data"),
    );
    expect(screen.getByRole("button", { name: "Querying…" })).toBeTruthy();
    act(() => server.release());

    expect(await screen.findByText(totalOf(newRows))).toBeTruthy();
    await waitFor(() => expect(server.runs()).toHaveLength(8));
    expect(server.runs().slice(4)).toEqual(server.runs().slice(0, 4));
    // The time series' chart was given the new rows.
    await waitFor(() => {
      const drawn = charts.filter((candidate) => !candidate.disposed && card("Power by line").contains(candidate.root));
      expect(JSON.stringify(drawn.at(-1)?.option.series)).toContain("99.5");
    });
    // Each card says when its data was queried.
    expect(within(card("Energy this week")).getByTestId("card-facts").textContent).toContain(
      `Queried at ${format.instant("2026-11-02T03:00:00Z")}`,
    );
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
    expect(within(whole).getByText("The whole result, 1 rows")).toBeTruthy();
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

  test("a custom page item is a placeholder: no frame, nothing from /sandbox/", async () => {
    const fetched: string[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
      fetched.push(input instanceof Request ? input.url : String(input));
      return originalFetch(input, init);
    }) as typeof fetch;
    try {
      const server = new FakeDashboardServer();
      const dashboard = server.addDashboard({
        versions: [[SPECS.kpi, { kind: "custom_page" }]],
        page: '<div id="chart"></div><script>tq.ready()</script>',
      });
      open(server, dashboardPath(dashboard.id));
      expect(await screen.findByText("This item can't be shown yet.")).toBeTruthy();
      expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
      expect(document.querySelector("iframe")).toBeNull();
      expect(document.querySelector('[src*="/sandbox"]')).toBeNull();
      expect(fetched.filter((url) => url.includes("/sandbox"))).toEqual([]);
      expect(server.requests.filter((request) => request.url.pathname.includes("/sandbox"))).toEqual([]);
      // The placeholder takes the custom page's place in the list.
      expect(
        [...document.querySelectorAll("[data-component]")].map((element) => element.getAttribute("data-component")),
      ).toEqual(["kpi", "custom_page"]);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  test("a version written before the component list shows its page's place, and an unknown item says so", async () => {
    const server = new FakeDashboardServer();
    const old = server.addDashboard({ versions: [[]] });
    const version = old.versions[0];
    if (version === undefined) throw new Error("no version");
    version.page = "<p>old page</p>";
    version.runtime = 1;
    delete version.spec.components;
    open(server, dashboardPath(old.id));
    expect(await screen.findByText("This item can't be shown yet.")).toBeTruthy();
    cleanup();

    const newer = server.addDashboard({ versions: [[{ kind: "gauge", title: "Pressure" }, SPECS.kpi]] });
    open(server, dashboardPath(newer.id));
    expect(await screen.findByRole("heading", { level: 3, name: "Item can't be shown" })).toBeTruthy();
    expect(await kpiValue()).toBe(totalOf(DAILY_ROWS));
  });
});

describe("versions, restore and archive", () => {
  test("the versions list links to each version; restoring an older one shows its components and datasets", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.timeSeries, SPECS.kpi]] });
    const { router } = open(server, dashboardPath(dashboard.id));
    await screen.findByText("Version 2 (latest)");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Versions" }));
    });
    const panel = await screen.findByRole("region", { name: "Versions" });
    const items = await within(panel).findAllByRole("listitem");
    expect(items.map((item) => item.getAttribute("aria-current"))).toEqual(["true", null]);
    act(() => {
      fireEvent.click(within(panel).getByRole("link", { name: "View version 1" }));
    });
    expect(await screen.findByText("Version 1 of 2")).toBeTruthy();
    expect(router.state.location.search).toBe("?version=1");
    expect(await screen.findByRole("heading", { level: 3, name: "Meters" })).toBeTruthy();

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Restore version 1" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    expect(document.activeElement?.textContent).toBe("Cancel");
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Restore version 1" }));
    });
    expect(await screen.findByText("Version 3 (latest)")).toBeTruthy();
    expect(screen.getByText("Restored version 1 as version 3.")).toBeTruthy();
    expect(router.state.location.search).toBe("");
    const restore = server.requests.find((request) => request.url.pathname.endsWith("/restore"));
    expect(restore?.body).toEqual({ from_version: 1, base_version: 2 });
    // Version 3 has version 1's components, and its datasets run under version 3.
    expect(await screen.findByRole("heading", { level: 3, name: "Meters" })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 3, name: "Power by line" })).toBeNull();
    await waitFor(() => expect(server.runs().filter((run) => run.version === 3)).toHaveLength(4));
  });

  test("a viewer sees neither restore nor archive; an editor sees both", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id, 1), { role: "viewer" });
    expect(await screen.findByText("Version 1 of 2")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Show the latest version" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^Restore/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Archive" })).toBeNull();
    cleanup();

    open(server, dashboardPath(dashboard.id, 1), { role: "editor" });
    expect(await screen.findByText("Version 1 of 2")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Restore version 1" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Archive" })).toBeTruthy();
  });

  test("a refused restore says why in the dialog, translated", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.table], [SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id, 1));
    await screen.findByText("Version 1 of 2");
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
  });

  test("archiving asks first, then marks the dashboard; unarchiving undoes it", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id));
    await screen.findByText("Version 1 (latest)");
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Archive" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));
    });
    expect(
      await screen.findByText("This dashboard is archived: it isn't in the dashboard list and gets no new versions."),
    ).toBeTruthy();
    expect(server.requests.some((request) => request.method === "PUT")).toBe(true);
    expect(dashboard.archived_at).toBeDefined();

    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Unarchive" }));
    });
    await waitFor(() => expect(screen.queryByText(/This dashboard is archived/)).toBeNull());
    expect(
      server.requests.filter((request) => request.url.pathname.endsWith("/archive")).map((request) => request.method),
    ).toEqual(["PUT", "DELETE"]);
    expect(dashboard.archived_at).toBeUndefined();
  });

  test("an archive refused by the server says why", async () => {
    const server = new FakeDashboardServer();
    const dashboard = server.addDashboard({ versions: [[SPECS.kpi]] });
    open(server, dashboardPath(dashboard.id));
    await screen.findByText("Version 1 (latest)");
    server.failNext("archive", fakeError(403, "permission_denied", "dashboard.editor_required"));
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Archive" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Archive" }));
    });
    expect(
      await within(dialog).findByText("Only editors and admins can publish, restore or archive dashboards."),
    ).toBeTruthy();
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
});
