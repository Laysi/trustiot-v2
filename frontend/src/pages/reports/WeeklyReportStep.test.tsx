import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor } from "@testing-library/react";

import { ReportApiProvider } from "../../api/pending/ReportApiProvider.tsx";
import { FakeQuestionServer } from "../../test/fake-questions.ts";
import { FakeReportServer } from "../../test/fake-reports.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { WEEKLY_REPORT_ONBOARDING_PATH } from "./paths.ts";
import { WeeklyReportStep } from "./WeeklyReportStep.tsx";

afterEach(() => {
  cleanup();
});

function show(server: FakeReportServer, role: "viewer" | "editor" | "admin" = "admin") {
  return renderAt("/", {
    identity: signedInBrowser({ role }).browser,
    conversations: new FakeQuestionServer().api(),
    routes: [
      {
        path: "/",
        element: (
          <ReportApiProvider api={server.api()}>
            <WeeklyReportStep />
          </ReportApiProvider>
        ),
      },
      { path: "/reports/schedules/new", element: <p>the form</p> },
    ],
  });
}

describe("the checklist step's option for reports", () => {
  test("offers the weekly report's form, filled in, next to the question for the AI", async () => {
    const { router } = show(new FakeReportServer());
    const next = await screen.findByRole("link", { name: "Next: set up the weekly report" });
    expect(next.getAttribute("href")).toBe(WEEKLY_REPORT_ONBOARDING_PATH);
    expect(screen.getByRole("button", { name: "What was this week's energy use?" })).toBeTruthy();
    act(() => {
      fireEvent.click(next);
    });
    await waitFor(() => expect(router.state.location.pathname).toBe("/reports/schedules/new"));
    expect(router.state.location.search).toBe("?preset=weekly&from=onboarding");
  });

  test("a plan without reports gets only the question, and no upgrade wall", async () => {
    const server = new FakeReportServer();
    server.plan = "starter";
    show(server);
    await screen.findByRole("button", { name: "What was this week's energy use?" });
    await waitFor(() => expect(server.calls.some((call) => call.endpoint === "listSchedules")).toBe(true));
    expect(screen.queryByRole("link", { name: "Next: set up the weekly report" })).toBeNull();
    expect(screen.queryByText(/Business plan/)).toBeNull();
  });

  test("a viewer gets only the question, and the reports are not even asked for", async () => {
    const server = new FakeReportServer();
    show(server, "viewer");
    await screen.findByRole("button", { name: "What was this week's energy use?" });
    expect(screen.queryByRole("link", { name: "Next: set up the weekly report" })).toBeNull();
    expect(server.calls).toHaveLength(0);
  });

  test("the question starts a conversation and sends it", async () => {
    const { router } = show(new FakeReportServer());
    const ask = await screen.findByRole("button", { name: "What was this week's energy use?" });
    act(() => {
      fireEvent.click(ask);
    });
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\//));
  });
});

describe("the reports pages in the product", () => {
  test("are only in the UI preview: not in the routes and not in the menu", async () => {
    const { ROUTES } = await import("../../app/routes.tsx");
    const { MENU_ITEMS } = await import("../../app/menu.ts");
    const paths = JSON.stringify(ROUTES, (_key, value) => (typeof value === "function" ? undefined : value));
    expect(paths).not.toContain("reports");
    expect(MENU_ITEMS.map((item) => item.id)).not.toContain("reports");
  });

  test("the preview declares its routes, its menu entry and its fake in the form the preview checks", async () => {
    const { collectFakes, collectPages } = await import("../../ui-preview/collect.ts");
    const { MENU_ITEMS } = await import("../../app/menu.ts");
    const fake = await import("../../test/fake-reports.ts");
    const pages = await import("./preview.ts");
    const fakes = collectFakes({ "../test/fake-reports.ts": fake });
    expect(fakes.map((entry) => entry.declaration.name)).toEqual(["reports"]);
    expect("provider" in (fakes[0]?.declaration ?? {})).toBe(true);
    const collected = collectPages({ "../pages/reports/preview.ts": pages }, MENU_ITEMS);
    expect(collected.routes.map((route) => route.path)).toEqual([
      "reports",
      "reports/schedules/new",
      "reports/schedules/:scheduleId/edit",
      "reports/:reportId",
    ]);
    expect(collected.menu.map((item) => [item.id, item.group, item.path])).toEqual([["reports", 3, "/reports"]]);
    const item = collected.menu[0];
    expect(item?.covers("/reports/schedules/new")).toBe(true);
    expect(item?.covers("/reportsx")).toBe(false);
  });

  test("the preview's fake seeds the normal state and an empty one", async () => {
    const { preview } = await import("../../test/fake-reports.ts");
    expect(typeof preview.provider).toBe("function");
    const normal = new FakeReportServer().seedNormal();
    expect(normal.schedules).toHaveLength(2);
    expect(normal.reports.map((report) => report.status).sort()).toEqual(["failed", "generating", "ready", "ready"]);
    expect(new FakeReportServer().schedules).toHaveLength(0);
  });
});
