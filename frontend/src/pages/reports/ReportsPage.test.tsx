import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { apiError } from "../../test/fake-identity.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { FakeReportServer } from "../../test/fake-reports.ts";
import { signedInBrowser } from "../../test/render.tsx";
import { recordingSaver, renderReports } from "../../test/render-reports.tsx";

afterEach(() => {
  cleanup();
});

/** The list polls every few seconds while a report is being made; the wait outlasts one poll. */
const POLL_WAIT = { timeout: 30_000 };
const SLOW_TEST_MS = 60_000;

const WEEKLY = "Weekly energy report";
const MONTHLY = "Monthly carbon report";

function open(server: FakeReportServer, role: "viewer" | "editor" | "admin" = "admin", path = "/reports") {
  const saver = recordingSaver();
  const rendered = renderReports(path, server, {
    identity: signedInBrowser({ role }).browser,
    conversations: new FakeQuestionServer().api(),
    saver,
  });
  return { ...rendered, saver };
}

/** The card of the schedule named `name`. */
function card(name: string): HTMLElement {
  return screen.getByRole("heading", { level: 3, name }).closest("section") as HTMLElement;
}

async function loaded() {
  await screen.findByRole("heading", { level: 2, name: "Schedules" });
}

describe("the reports page", () => {
  test("lists the schedules as cards and the generated reports in a table", async () => {
    const server = new FakeReportServer().seedNormal();
    open(server);
    await loaded();
    const weekly = card(WEEKLY);
    expect(within(weekly).getByText("3 recipients")).toBeTruthy();
    expect(within(weekly).getByText("Every Monday at 08:00")).toBeTruthy();
    expect(within(weekly).getByText(/last sent/)).toBeTruthy();
    expect(
      within(weekly)
        .getByRole("switch", { name: `Turn on ${WEEKLY}` })
        .getAttribute("aria-checked"),
    ).toBe("true");
    expect(within(card(MONTHLY)).getByText(/factor 0\.494/)).toBeTruthy();

    const table = screen.getByRole("table", { name: /Generated reports/ });
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(4);
    const statuses = rows.map((row) => within(row).queryByText(/^(Done|Generating|Failed)$/)?.textContent);
    expect(statuses.sort()).toEqual(["Done", "Done", "Failed", "Generating"]);
    // A report that is ready is a link; one being made is not.
    expect(within(table).getAllByRole("link")).toHaveLength(2);
    expect(within(table).getByText("The query timed out; nothing was sent.")).toBeTruthy();
  });

  test("a recipient whose mail bounced is named on the schedule, with the way to fix it", async () => {
    const server = new FakeReportServer().seedNormal();
    open(server);
    await loaded();
    const monthly = card(MONTHLY);
    expect(within(monthly).getByText(/plant-mgr@example\.com/).textContent).toContain("bounced");
    const link = within(monthly).getByRole("link", { name: "Change recipients" });
    const edit = server.schedules.find((schedule) => schedule.kind === "monthly_carbon");
    expect(link.getAttribute("href")).toBe(`/reports/schedules/${edit?.id}/edit`);
  });

  test("the switch turns a schedule off and on, and says what happened", async () => {
    const server = new FakeReportServer().seedNormal();
    open(server);
    await loaded();
    const toggle = within(card(WEEKLY)).getByRole("switch");
    act(() => {
      fireEvent.click(toggle);
    });
    await waitFor(() => expect(within(card(WEEKLY)).getByRole("switch").getAttribute("aria-checked")).toBe("false"));
    expect(screen.getByText(/Turned off Weekly energy report/)).toBeTruthy();
    expect(within(card(WEEKLY)).getByText(/Paused/)).toBeTruthy();
    expect(server.calls.filter((call) => call.endpoint === "setScheduleEnabled").map((call) => call.args)).toEqual([
      [server.schedules[0]?.id, false],
    ]);
    act(() => {
      fireEvent.click(within(card(WEEKLY)).getByRole("switch"));
    });
    await waitFor(() => expect(within(card(WEEKLY)).getByRole("switch").getAttribute("aria-checked")).toBe("true"));
    expect(screen.getByText(/Turned on Weekly energy report/)).toBeTruthy();
  });

  test(
    "making one now adds a report that is being made, and the list follows it until it is ready",
    async () => {
      const server = new FakeReportServer();
      server.addSchedule({ kind: "weekly_energy" });
      open(server);
      await loaded();
      act(() => {
        fireEvent.click(screen.getByRole("button", { name: `Make a ${WEEKLY} now` }));
      });
      expect((await screen.findAllByText(/Started a Weekly energy report for/))[0]).toBeTruthy();
      const table = await screen.findByRole("table", { name: /Generated reports/ });
      expect(within(table).getByText("Generating")).toBeTruthy();
      act(() => server.finish());
      await waitFor(() => expect(within(screen.getByRole("table")).getByText("Done")).toBeTruthy(), POLL_WAIT);
      expect(within(screen.getByRole("table")).queryByText("Generating")).toBeNull();
    },
    SLOW_TEST_MS,
  );

  test("deleting a schedule asks first, with Cancel focused, and keeps the reports", async () => {
    const server = new FakeReportServer().seedNormal();
    open(server);
    await loaded();
    act(() => {
      fireEvent.pointerDown(within(card(MONTHLY)).getByRole("button", { name: `More actions for ${MONTHLY}` }), {
        button: 0,
        ctrlKey: false,
      });
    });
    act(() => {
      fireEvent.click(screen.getByRole("menuitem", { name: "Delete schedule" }));
    });
    const dialog = await screen.findByRole("alertdialog", { name: `Delete the schedule "${MONTHLY}"?` });
    await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" })));
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete schedule" }));
    });
    await waitFor(() => expect(screen.queryByRole("heading", { level: 3, name: MONTHLY })).toBeNull());
    expect(screen.getByText(`Deleted the schedule "${MONTHLY}".`)).toBeTruthy();
    // The reports it made stay.
    expect(within(screen.getByRole("table")).getAllByRole("row").slice(1)).toHaveLength(4);
  });

  test("a refused deletion is shown in the dialog, which stays open", async () => {
    const server = new FakeReportServer().seedNormal();
    open(server);
    await loaded();
    server.failNext("deleteSchedule", apiError(500, "internal", "error.internal"));
    act(() => {
      fireEvent.pointerDown(within(card(WEEKLY)).getByRole("button", { name: `More actions for ${WEEKLY}` }), {
        button: 0,
        ctrlKey: false,
      });
    });
    act(() => {
      fireEvent.click(screen.getByRole("menuitem", { name: "Delete schedule" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    act(() => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete schedule" }));
    });
    expect((await within(dialog).findByRole("alert")).textContent).toContain("Something went wrong");
    // The page behind the dialog is hidden from assistive technology while it is open.
    expect(screen.getByRole("heading", { level: 3, name: WEEKLY, hidden: true })).toBeTruthy();
  });

  test("a failed report can be tried again, and it is being made again", async () => {
    const server = new FakeReportServer().seedNormal();
    open(server);
    await loaded();
    const table = await screen.findByRole("table", { name: /Generated reports/ });
    const failed = within(table).getByText("Failed").closest("tr") as HTMLElement;
    act(() => {
      fireEvent.click(within(failed).getByRole("button", { name: /^Try making Weekly energy report/ }));
    });
    await waitFor(() => expect(screen.getByText(/Making the Weekly energy report for/)).toBeTruthy());
    await waitFor(() => expect(within(screen.getByRole("table")).queryByText("Failed")).toBeNull());
    expect(server.calls.some((call) => call.endpoint === "retryReport")).toBe(true);
  });

  test("a download fetches the file and saves it under a name the page builds", async () => {
    const server = new FakeReportServer().seedNormal();
    const { saver } = open(server);
    await loaded();
    const monthly = server.reports.find((report) => report.kind === "monthly_carbon");
    const table = await screen.findByRole("table", { name: /Generated reports/ });
    const row = within(table).getByRole("link", { name: MONTHLY }).closest("tr") as HTMLElement;
    act(() => {
      fireEvent.click(within(row).getByRole("button", { name: /^Download the CSV of/ }));
    });
    await waitFor(() => expect(saver.saved).toHaveLength(1));
    expect(saver.saved[0]?.name).toBe(`trustiot-monthly-carbon-${monthly?.period_start}.csv`);
    expect(saver.saved[0]?.type).toBe("text/csv");
    expect(saver.saved[0]?.text.startsWith("date,kwh\n")).toBe(true);
    expect(server.downloads).toEqual([{ reportId: monthly?.id ?? "", format: "csv" }]);
  });

  test("a download that fails says so and saves nothing", async () => {
    const server = new FakeReportServer().seedNormal();
    const { saver } = open(server);
    await loaded();
    server.failNext("download", apiError(500, "internal", "error.internal"));
    const table = await screen.findByRole("table", { name: /Generated reports/ });
    const row = within(table).getByRole("link", { name: MONTHLY }).closest("tr") as HTMLElement;
    act(() => {
      fireEvent.click(within(row).getByRole("button", { name: /^Download the PDF of/ }));
    });
    expect(await screen.findByText(/Couldn't download the file/)).toBeTruthy();
    expect(saver.saved).toHaveLength(0);
  });

  test("the type filter narrows the list and is kept in the address", async () => {
    const server = new FakeReportServer().seedNormal();
    const { router } = open(server);
    await loaded();
    await screen.findByRole("table", { name: /Generated reports/ });
    act(() => {
      fireEvent.change(screen.getByRole("combobox", { name: "Report type" }), { target: { value: "monthly_carbon" } });
    });
    await waitFor(() => expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2));
    expect(router.state.location.search).toBe("?kind=monthly_carbon");
    expect(server.calls.at(-1)?.args[0]).toMatchObject({ kind: "monthly_carbon" });
  });

  test("shows the empty state with both ways to start, and loading and failure states", async () => {
    const server = new FakeReportServer();
    server.hold("listSchedules");
    open(server);
    await waitFor(() =>
      expect(screen.getAllByRole("status").some((node) => node.textContent === "Loading reports…")).toBe(true),
    );
    act(() => server.release());
    expect(await screen.findByRole("heading", { level: 2, name: "No reports yet" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Set up the weekly report" }).getAttribute("href")).toBe(
      "/reports/schedules/new?kind=weekly_energy",
    );
    expect(screen.getByRole("link", { name: "Set up the monthly carbon report" }).getAttribute("href")).toBe(
      "/reports/schedules/new?kind=monthly_carbon",
    );
  });

  test("a failed load says what still works and can be retried", async () => {
    const server = new FakeReportServer().seedNormal();
    server.failNext("listSchedules", apiError(500, "internal", "error.internal"));
    open(server);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Couldn't load the reports");
    expect(alert.textContent).toContain("Schedules already set up still make and send their reports on time.");
    act(() => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    await loaded();
    expect(card(WEEKLY)).toBeTruthy();
  });
});

describe("a plan without reports", () => {
  test("shows the upgrade wall with what can be done now, and no way to add a schedule", async () => {
    const server = new FakeReportServer();
    server.plan = "starter";
    open(server, "admin");
    expect(await screen.findByRole("heading", { level: 2, name: /Business plan and above/ })).toBeTruthy();
    expect(screen.getByText("Contact us to upgrade your plan.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "What was last month's energy use?" })).toBeTruthy();
    expect(screen.queryByRole("link", { name: "New schedule" })).toBeNull();
  });

  test("a member who is not an administrator is told to ask one", async () => {
    const server = new FakeReportServer();
    server.plan = "starter";
    open(server, "editor");
    expect(await screen.findByText("Ask an administrator of your organization to upgrade the plan.")).toBeTruthy();
  });

  test("the wall's question starts a conversation and sends it", async () => {
    const server = new FakeReportServer();
    server.plan = "starter";
    const { router } = open(server, "admin");
    const ask = await screen.findByRole("button", { name: "What was last month's energy use?" });
    act(() => {
      fireEvent.click(ask);
    });
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\/[0-9a-f-]{36}$/));
    expect(router.state.location.state).toMatchObject({ asked: { text: "What was last month's energy use?" } });
  });
});

describe("roles", () => {
  test("an editor sets up schedules", async () => {
    const server = new FakeReportServer().seedNormal();
    open(server, "editor");
    await loaded();
    expect(screen.getByRole("link", { name: "New schedule" }).getAttribute("href")).toBe("/reports/schedules/new");
    expect(within(card(WEEKLY)).getByRole("link", { name: `Edit ${WEEKLY}` })).toBeTruthy();
  });

  test("a viewer reads and downloads, and sees none of the schedule's controls", async () => {
    const server = new FakeReportServer().seedNormal();
    open(server, "viewer");
    await loaded();
    expect(screen.getByText("You're a viewer")).toBeTruthy();
    expect(screen.queryByRole("link", { name: "New schedule" })).toBeNull();
    expect(screen.queryByRole("switch")).toBeNull();
    expect(screen.queryByRole("link", { name: /^Edit / })).toBeNull();
    expect(screen.queryByRole("button", { name: /Make a .* now/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /^More actions/ })).toBeNull();
    expect(screen.queryByRole("link", { name: "Change recipients" })).toBeNull();
    expect(within(card(WEEKLY)).getByText("On")).toBeTruthy();
    // No retry on the failed report; the downloads are there.
    expect(screen.queryByRole("button", { name: /^Try making/ })).toBeNull();
    expect(screen.getAllByRole("button", { name: /^Download the PDF of/ }).length).toBeGreaterThan(0);
  });

  test("a viewer's empty page says an editor can set up a schedule", async () => {
    open(new FakeReportServer(), "viewer");
    expect(await screen.findByText(/An editor or admin can set up schedules/)).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Set up the weekly report" })).toBeNull();
  });
});
