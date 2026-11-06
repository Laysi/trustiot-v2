import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { formatDateTime } from "../../i18n/format.ts";
import { FakeSetupServer } from "../../test/fake-setup.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { homeRoutes } from "./test-support.tsx";

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

function renderHome(server: FakeSetupServer, options: { role?: "viewer" | "editor" | "admin"; pages?: string[] } = {}) {
  return renderAt("/", {
    routes: homeRoutes({ setup: server.api(), pages: options.pages ?? ["/devices"] }),
    identity: signedInBrowser({ role: options.role ?? "admin" }).browser,
  });
}

async function checklist() {
  return await screen.findByRole("region", { name: "Get started with TrustIoT" });
}

/** The ordered list of steps (a current step's choices are a list of their own). */
function stepsOf(region: HTMLElement): HTMLElement {
  const list = region.querySelector("ol");
  if (!list) throw new Error("no step list");
  return list;
}

function stepNamed(list: HTMLElement, name: RegExp | string) {
  const item = within(list)
    .getAllByRole("listitem")
    .find((candidate) =>
      typeof name === "string" ? candidate.textContent?.includes(name) : name.test(candidate.textContent ?? ""),
    );
  if (!item) throw new Error(`no step ${String(name)}`);
  return item;
}

describe("the setup checklist on the home page", () => {
  test("a new tenant sees five steps, the first one open, and no link to a page that does not exist", async () => {
    renderHome(new FakeSetupServer());
    const region = await checklist();
    expect(within(region).getByText("0 of 5 steps done. Every step can be skipped.")).toBeTruthy();
    const steps = within(stepsOf(region)).getAllByRole("listitem");
    expect(steps).toHaveLength(5);
    // Exactly the first step is the current one, and it says so in words as well.
    expect(steps.map((step) => step.getAttribute("aria-current"))).toEqual(["step", null, null, null, null]);
    expect(within(steps[0] as HTMLElement).getByText("(current step)")).toBeTruthy();
    expect(within(steps[1] as HTMLElement).getByText("(not started)")).toBeTruthy();
    expect(within(steps[0] as HTMLElement).getByText(/binding code from the label/)).toBeTruthy();
    expect(within(steps[0] as HTMLElement).getByText(/joins Acme Factory/)).toBeTruthy();
    // The gateway page is not in this build's routes: no button leads there, the skip button stays.
    expect(within(region).queryByRole("link")).toBeNull();
    expect(within(steps[0] as HTMLElement).getByRole("button", { name: /Skip this step/ })).toBeTruthy();
    // The home page's own content is still there, below it.
    expect(screen.getByRole("heading", { level: 1, name: "Home" })).toBeTruthy();
  });

  test("a step links to its page when the router has it", async () => {
    renderHome(FakeSetupServer.midway(), { pages: ["/devices", "/members"] });
    const region = await checklist();
    const link = within(region).getByRole("link", { name: /Next: check device connections/ });
    expect(link.getAttribute("href")).toBe("/devices");
  });

  test("a dynamic route does not count as the page of a step", async () => {
    const server = new FakeSetupServer();
    server.complete("bind_gateway", "2026-11-01T09:12:00Z");
    // "/devices/new" would match this, and open a device called "new".
    renderHome(server, { pages: ["/devices", "/devices/:deviceId"] });
    const region = await checklist();
    expect(stepNamed(stepsOf(region), "Add a device").getAttribute("aria-current")).toBe("step");
    expect(within(region).queryByRole("link", { name: /Next: add a device/ })).toBeNull();
    cleanup();

    renderHome(server, { pages: ["/devices", "/devices/:deviceId", "/devices/new"] });
    const again = await checklist();
    expect(
      within(again)
        .getByRole("link", { name: /Next: add a device/ })
        .getAttribute("href"),
    ).toBe("/devices/new");
  });

  test("a tenant midway shows the finished steps with their time, and the third step waiting for a reading", async () => {
    renderHome(FakeSetupServer.midway());
    const region = await checklist();
    expect(within(region).getByText("2 of 5 steps done. Every step can be skipped.")).toBeTruthy();
    const list = stepsOf(region);
    const bound = stepNamed(list, "Bind a gateway");
    expect(within(bound).getByText("(done)")).toBeTruthy();
    expect(within(bound).getByText(`Done ${formatDateTime("2026-11-01T09:12:00Z", "en", "UTC")}`)).toBeTruthy();
    expect(within(stepNamed(list, "Add a device")).getByText(/, 3 devices$/)).toBeTruthy();
    const waiting = stepNamed(list, "See the first reading");
    expect(waiting.getAttribute("aria-current")).toBe("step");
    expect(within(waiting).getByText(/3 devices added/)).toBeTruthy();
    const status = within(waiting).getByRole("status");
    expect(status.textContent).toBe(
      `Waiting for the first reading. Last checked ${formatDateTime("2026-11-01T18:24:00Z", "en", "UTC")}`,
    );
    // Nothing the person can do completes this step: no "mark as done" anywhere.
    expect(within(region).queryByRole("button", { name: /done/i })).toBeNull();
  });

  test("skipping a step shows it skipped only after the server did, and moves focus to the next step", async () => {
    const server = FakeSetupServer.midway();
    renderHome(server);
    const region = await checklist();
    act(() => {
      fireEvent.click(within(region).getByRole("button", { name: "Skip this step: See the first reading" }));
    });
    await waitFor(() => expect(server.calls).toContain("skip:first_data"));
    const list = stepsOf(region);
    await waitFor(() => expect(within(stepNamed(list, "See the first reading")).getByText("Skipped")).toBeTruthy());
    expect(within(region).getByText("2 of 5 steps done, 1 skipped. Every step can be skipped.")).toBeTruthy();
    // The first use is now the current step: its choices show, and focus is on its skip button.
    const use = stepNamed(list, "Pick a first use");
    expect(use.getAttribute("aria-current")).toBe("step");
    expect(within(use).getByRole("button", { name: /See live values and charts/ })).toBeTruthy();
    await waitFor(() =>
      expect((document.activeElement as HTMLElement).getAttribute("aria-label")).toBe(
        "Skip this step: Pick a first use",
      ),
    );
  });

  test("a failed skip leaves the step as it was and says why inside it", async () => {
    const server = FakeSetupServer.midway();
    server.failNext("skip");
    renderHome(server);
    const region = await checklist();
    const skip = within(region).getByRole("button", { name: "Skip this step: See the first reading" });
    act(() => {
      fireEvent.click(skip);
    });
    const alert = await within(region).findByRole("alert");
    expect(alert.textContent).toContain("Not skipped: The service is temporarily unavailable.");
    const waiting = stepNamed(stepsOf(region), "See the first reading");
    expect(waiting.getAttribute("aria-current")).toBe("step");
    expect(waiting.contains(alert)).toBe(true);
    expect(within(region).queryByText("Skipped")).toBeNull();
  });

  test("a skipped step can be taken back", async () => {
    const server = FakeSetupServer.midway();
    renderHome(server);
    const region = await checklist();
    act(() => {
      fireEvent.click(within(region).getByRole("button", { name: "Skip this step: See the first reading" }));
    });
    const undo = await within(region).findByRole("button", { name: "Undo skipping: See the first reading" });
    act(() => {
      fireEvent.click(undo);
    });
    await waitFor(() => expect(server.calls).toContain("restore:first_data"));
    await waitFor(() =>
      expect(stepNamed(stepsOf(region), "See the first reading").getAttribute("aria-current")).toBe("step"),
    );
    expect(within(region).queryByText("Skipped")).toBeNull();
  });

  test("choosing the first use completes that step", async () => {
    const server = FakeSetupServer.midway();
    server.complete("first_data", "2026-11-01T09:46:00Z");
    renderHome(server);
    const region = await checklist();
    const list = stepsOf(region);
    const cards = within(stepNamed(list, "Pick a first use")).getAllByRole("button", { name: /^(See live|Export)/ });
    expect(cards.map((card) => card.textContent)).toEqual([
      "See live values and chartsOpen a device to read its latest values and chart its history.",
      "Export CSVTake a period of readings out as a CSV file.Needs the Business plan",
    ]);
    act(() => {
      fireEvent.click(cards[0] as HTMLElement);
    });
    await waitFor(() => expect(server.calls).toContain("choose:live_values"));
    await waitFor(() => expect(within(stepNamed(list, "Pick a first use")).getByText("(done)")).toBeTruthy());
    // The last open step is now the invitation.
    expect(stepNamed(list, "Invite members").getAttribute("aria-current")).toBe("step");
    expect(within(stepNamed(list, "Invite members")).getByText(/allows 4 more members/)).toBeTruthy();
  });

  test("when every step is done or skipped a one-time notice replaces the list, and closing it ends it", async () => {
    const server = FakeSetupServer.midway();
    server.complete("first_data", "2026-11-01T09:46:00Z");
    server.complete("first_use", "2026-11-01T10:00:00Z");
    renderHome(server);
    const region = await checklist();
    act(() => {
      fireEvent.click(within(region).getByRole("button", { name: "Skip this step: Invite members" }));
    });
    const notice = await screen.findByRole("region", { name: "Setup complete" });
    expect(notice.textContent).toContain("4 steps done, 1 skipped (Invite members).");
    expect(screen.queryByRole("region", { name: "Get started with TrustIoT" })).toBeNull();
    await waitFor(() =>
      expect(document.activeElement).toBe(
        within(notice).getByRole("button", { name: "Close the setup complete notice" }),
      ),
    );
    act(() => {
      fireEvent.click(within(notice).getByRole("button", { name: "Close the setup complete notice" }));
    });
    await waitFor(() => expect(server.calls).toContain("dismiss"));
    await waitFor(() => expect(screen.queryByRole("region", { name: "Setup complete" })).toBeNull());
  });

  test("a closed notice stays closed on the next visit, and the checklist is not drawn at all", async () => {
    const server = FakeSetupServer.midway();
    for (const step of ["first_data", "first_use", "invite_members"] as const)
      server.complete(step, "2026-11-01T10:00:00Z");
    server.state.completion_dismissed = true;
    renderHome(server);
    expect(await screen.findByRole("heading", { level: 1, name: "Home" })).toBeTruthy();
    await waitFor(() => expect(server.calls).toEqual(["get"]));
    expect(screen.queryByRole("region", { name: "Get started with TrustIoT" })).toBeNull();
    expect(screen.queryByRole("region", { name: "Setup complete" })).toBeNull();
  });

  test("a step the owning service completes while the page is open ticks and is announced", async () => {
    const server = FakeSetupServer.midway();
    const { queryClient } = renderHome(server);
    const region = await checklist();
    server.complete("first_data", "2026-11-04T10:30:00Z");
    await act(async () => {
      await queryClient.invalidateQueries({ queryKey: ["setup"] });
    });
    const list = stepsOf(region);
    await waitFor(() => expect(within(stepNamed(list, "See the first reading")).getByText("(done)")).toBeTruthy());
    expect(within(region).getByText("Done: See the first reading")).toBeTruthy();
    expect(stepNamed(list, "Pick a first use").getAttribute("aria-current")).toBe("step");
  });

  test("only a tenant admin sees it, and others do not even ask for it", async () => {
    for (const role of ["editor", "viewer"] as const) {
      const server = FakeSetupServer.midway();
      renderHome(server, { role });
      expect(await screen.findByRole("heading", { level: 1, name: "Home" })).toBeTruthy();
      expect(screen.queryByRole("region", { name: "Get started with TrustIoT" })).toBeNull();
      expect(server.calls).toEqual([]);
      cleanup();
    }
  });

  test("without setup endpoints (the production build) the home page has no checklist", async () => {
    renderAt("/");
    expect(await screen.findByRole("heading", { level: 1, name: "Home" })).toBeTruthy();
    expect(screen.queryByRole("region", { name: "Get started with TrustIoT" })).toBeNull();
  });

  test("a failed read is one line with a retry, and the home page stays", async () => {
    const server = FakeSetupServer.midway();
    server.failNext("get");
    renderHome(server);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The setup checklist can't be loaded right now");
    expect(screen.getByRole("heading", { level: 1, name: "Home" })).toBeTruthy();
    act(() => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    await checklist();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  test("after three failed reads in a row the checklist stays away", async () => {
    const server = FakeSetupServer.midway();
    for (let attempt = 0; attempt < 3; attempt += 1) server.failNext("get");
    renderHome(server);
    for (let attempt = 0; attempt < 2; attempt += 1) {
      const alert = await screen.findByRole("alert");
      await waitFor(() =>
        expect(within(alert).getByRole("button", { name: "Try again" }).hasAttribute("disabled")).toBe(false),
      );
      act(() => {
        fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
      });
      await waitFor(() => expect(server.calls.length).toBe(attempt + 2));
    }
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
    expect(screen.queryByRole("region", { name: "Get started with TrustIoT" })).toBeNull();
  });

  test("collapsing keeps the progress line, and the choice is remembered for this person and tenant", async () => {
    const server = FakeSetupServer.midway();
    renderHome(server);
    const region = await checklist();
    const toggle = within(region).getByRole("button", { name: "Collapse" });
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    act(() => {
      fireEvent.click(toggle);
    });
    expect(within(region).queryByRole("list")).toBeNull();
    expect(within(region).getByText(/2 of 5 steps done/)).toBeTruthy();
    expect(within(region).getByRole("button", { name: "Expand" }).getAttribute("aria-expanded")).toBe("false");
    cleanup();
    renderHome(server);
    const again = await checklist();
    expect(within(again).queryByRole("list")).toBeNull();
    act(() => {
      fireEvent.click(within(again).getByRole("button", { name: "Expand" }));
    });
    expect(stepsOf(again)).toBeTruthy();
  });

  test("on a phone only the current step shows, and all steps open on request", async () => {
    const original = window.matchMedia;
    window.matchMedia = ((query: string) => ({
      matches: false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    })) as unknown as typeof window.matchMedia;
    try {
      renderHome(FakeSetupServer.midway());
      const region = await checklist();
      expect(within(region).getByText("2 of 5 steps done")).toBeTruthy();
      expect(within(stepsOf(region)).getAllByRole("listitem")).toHaveLength(1);
      const all = within(region).getByRole("button", { name: "All steps" });
      expect(all.getAttribute("aria-expanded")).toBe("false");
      act(() => {
        fireEvent.click(all);
      });
      expect(within(stepsOf(region)).getAllByRole("listitem")).toHaveLength(5);
      expect(within(region).getByRole("button", { name: "Current step only" }).getAttribute("aria-expanded")).toBe(
        "true",
      );
    } finally {
      window.matchMedia = original;
    }
  });
});
