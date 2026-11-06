import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { FakeReportServer } from "../../test/fake-reports.ts";
import { FakeSetupServer } from "../../test/fake-setup.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { homeRoutes } from "./test-support.tsx";

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

/** A tenant whose first reading is in: the step "choose a first use" is the current one. */
function atFirstUse() {
  const setup = FakeSetupServer.midway();
  setup.complete("first_data", "2026-11-01T19:00:00Z");
  return setup;
}

function renderHome(setup: FakeSetupServer, reports?: FakeReportServer) {
  return renderAt("/", {
    routes: homeRoutes({ setup: setup.api(), ...(reports ? { reports: reports.api() } : {}) }),
    identity: signedInBrowser({ role: "admin" }).browser,
  });
}

async function firstUseStep(): Promise<HTMLElement> {
  const region = await screen.findByRole("region", { name: "Get started with TrustIoT" });
  const step = within(region)
    .getAllByRole("listitem")
    .find((item) => item.getAttribute("aria-current") === "step");
  if (!step) throw new Error("no current step");
  return step;
}

describe("the weekly report in the setup checklist's step 4", () => {
  test("is offered next to the uses when the report pages are provided (the UI preview)", async () => {
    renderHome(atFirstUse(), new FakeReportServer());
    const step = await firstUseStep();
    expect(within(step).getByText("Pick a first use")).toBeTruthy();
    expect(await within(step).findByRole("link", { name: "Next: set up the weekly report" })).toBeTruthy();
    // The question for the AI is the shared button, the same one every other page uses.
    expect(within(step).getByRole("button", { name: "What was this week's energy use?" })).toBeTruthy();
  });

  test("is not there without them (the production build)", async () => {
    renderHome(atFirstUse());
    const step = await firstUseStep();
    expect(within(step).getByText("See live values and charts")).toBeTruthy();
    expect(within(step).queryByRole("link", { name: /weekly report/ })).toBeNull();
    expect(within(step).queryByRole("button", { name: "What was this week's energy use?" })).toBeNull();
  });

  test("saving the form returns home with the step ticked, once", async () => {
    const setup = atFirstUse();
    const reports = new FakeReportServer();
    const { router } = renderHome(setup, reports);
    const step = await firstUseStep();
    act(() => {
      fireEvent.click(within(step).getByRole("link", { name: "Next: set up the weekly report" }));
    });
    await screen.findByRole("form", { name: "Set up the weekly report" });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Save and finish this step" }));
    });
    await waitFor(() => expect(setup.calls).toContain("choose:weekly_report"));
    expect(router.state.location.pathname).toBe("/");
    expect(reports.schedules).toHaveLength(1);
    const region = await screen.findByRole("region", { name: "Get started with TrustIoT" });
    await waitFor(() => {
      const done = within(region)
        .getAllByRole("listitem")
        .find((item) => item.textContent?.includes("Pick a first use"));
      expect(done?.textContent).toContain("(done)");
    });
    // The note is off the address entry, so a reload does not choose again.
    expect(router.state.location.state).toBeNull();
    expect(setup.calls.filter((call) => call.startsWith("choose"))).toHaveLength(1);
  });
});
