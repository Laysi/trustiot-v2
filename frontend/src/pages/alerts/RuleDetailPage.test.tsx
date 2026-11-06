import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { renderAlerts } from "../../test/alerts-render.tsx";
import { FakeAlertsServer, fakeBacktest } from "../../test/fake-alerts.ts";
import { FakeQuestionServer } from "../../test/fake-questions.ts";

afterEach(() => {
  cleanup();
});

const PRESSURE = "/alerts/rules/rule-pressure";

async function open(path: string, options: Parameters<typeof renderAlerts>[2] = {}, server = new FakeAlertsServer()) {
  const rendered = renderAlerts(path, server, options);
  // A dialog that opens with the page hides it from the accessibility tree, hence `hidden`.
  await screen.findByRole("heading", {
    level: 1,
    hidden: true,
    name: /Compressor outlet pressure too low|Rule not found|Injection machine offline/,
  });
  return { server, ...rendered };
}

describe("a rule's page", () => {
  test("asks the assistant about the rule, which is written after the question, and leads back to it", async () => {
    const { router } = await open(PRESSURE, { conversations: new FakeQuestionServer().api() });
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Explain this rule?" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/conversations\/[0-9a-f-]{36}$/));
    const state = router.state.location.state as { asked: { text: string } };
    expect(state.asked.text).toBe("Explain this rule? (Rule: Compressor outlet pressure too low, Version: v3)");
    expect(await screen.findByRole("link", { name: /Compressor outlet pressure too low/ })).toBeTruthy();
  });

  test("shows who made the rule, its version, what it does, and the script folded away", async () => {
    await open(PRESSURE);
    const header = screen.getByRole("heading", { level: 1 }).closest("header") as HTMLElement;
    expect(within(header).getByText("AI-made")).toBeDefined();
    expect(within(header).getByText("Active")).toBeDefined();
    expect(within(header).getByText(/v3 · changed by AI on /)).toBeDefined();
    expect(within(header).getByRole("button", { name: "Explain this rule?" })).toBeDefined();
    expect(within(header).getByRole("link", { name: "Rules" }).getAttribute("href")).toBe("/alerts?tab=rules");

    const what = screen.getByRole("region", { name: "What the rule does" });
    expect(what.textContent).toContain("Notifies when the outlet pressure stays below 6.2 bar for 5 minutes");
    const details = what.querySelector("details") as HTMLDetailsElement;
    expect(details.open).toBe(false);
    expect(details.textContent).toContain('point("outlet_pressure")');

    const notify = screen.getByRole("region", { name: "Notifications" });
    expect(notify.textContent).toContain("15 minutes");
    expect(notify.textContent).toContain("4 notifications");
    expect(notify.textContent).toContain("Notification center");
    expect(notify.textContent).toContain("Webhook: Maintenance system");
    expect(
      within(screen.getByRole("region", { name: "Applies to" }))
        .getByRole("link", { name: "Compressor A2 · outlet_pressure" })
        .getAttribute("href"),
    ).toBe("/devices/dev-a2?point=outlet_pressure");
    const recent = screen.getByRole("region", { name: "Recent triggers" });
    expect(recent.textContent).toContain("Compressor A2 · 5.8 bar");
    expect(within(recent).getByRole("button", { name: "Compare with last week?" })).toBeDefined();
  });

  test("the backtest lists the four checks, the chart with its sentence, and the triggers as a table", async () => {
    await open(PRESSURE);
    const backtest = screen.getByRole("region", { name: "Backtest result" });
    expect(within(backtest).getByText("Passed")).toBeDefined();
    expect(backtest.textContent).toContain("The syntax is correct");
    expect(backtest.textContent).toContain("It ran without errors");
    expect(backtest.textContent).toContain("It fired 6 times in 14 days (limit 50)");
    expect(backtest.textContent).toContain("2 of 2 cases that should fire did");
    expect(backtest.textContent).toContain("a synthetic stretch");

    const chart = within(backtest).getByRole("img", { name: "Triggers per day" });
    expect(chart.querySelectorAll("rect")).toHaveLength(14);
    expect(chart.hasAttribute("style")).toBe(false);
    const described = chart.getAttribute("aria-describedby") as string;
    expect(document.getElementById(described)?.textContent).toMatch(/\d\d-\d\d: \d+.*; 0 on the other days\./);

    const toggle = within(backtest).getByRole("button", { name: "View each trigger as a table" });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(toggle);
    const table = within(backtest).getByRole("table", { name: "Triggers in the backtest, 6" });
    expect(within(table).getAllByRole("row")).toHaveLength(7);
    fireEvent.click(within(backtest).getByRole("button", { name: "Hide the table of triggers" }));
    expect(within(backtest).queryByRole("table")).toBeNull();
  });

  test("says when the history was shorter than 14 days and synthetic stretches filled in", async () => {
    const server = new FakeAlertsServer();
    server.addRule({
      id: "rule-new",
      name: "Compressor outlet pressure too low",
      versions: [
        {
          version: 1,
          author: { kind: "ai" },
          created_at: new Date().toISOString(),
          summary: "First.",
          script: "x",
          backtest: fakeBacktest(Date.now(), { historyDays: 9 }),
        },
      ],
    });
    await open("/alerts/rules/rule-new", { seed: "none" }, server);
    expect(
      screen.getByText(
        "The devices have only 9 days of history, so synthetic stretches were added for the cases that should fire.",
      ),
    ).toBeDefined();
    expect(screen.getByText(/v1 · created by AI on /)).toBeDefined();
  });

  test("a failed check is marked with an icon and the word, not only a colour", async () => {
    const server = new FakeAlertsServer();
    server.addRule({
      id: "rule-bad",
      name: "Compressor outlet pressure too low",
      status: "backtest_failed",
      backtest_failure: "expected_case",
      versions: [
        {
          version: 1,
          author: { kind: "ai" },
          created_at: new Date().toISOString(),
          summary: "First.",
          script: "x",
          backtest: fakeBacktest(Date.now(), {
            passed: false,
            triggerDaysAgo: [],
            cases: [{ source: "synthetic", triggered: false }],
          }),
        },
      ],
    });
    await open("/alerts/rules/rule-bad", { seed: "none" }, server);
    const backtest = screen.getByRole("region", { name: "Backtest result" });
    expect(within(backtest).getByText("Failed")).toBeDefined();
    expect(backtest.textContent).toContain("Failed: 0 of 1 cases that should fire did");
    expect(backtest.textContent).toContain("No trigger on any of the 14 days.");
    expect(screen.getAllByText("Backtest failed").length).toBeGreaterThan(0);
    // A rule that never ran cannot be disabled or enabled.
    expect(screen.queryByRole("button", { name: "Disable rule" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Enable rule" })).toBeNull();
  });
});

describe("the version history", () => {
  test("lists the versions newest first and compares a version with the one before", async () => {
    await open(PRESSURE);
    const versions = screen.getByRole("region", { name: "Version history" });
    const items = within(versions).getAllByRole("listitem");
    expect(items.map((item) => item.id)).toEqual(["version-3", "version-2", "version-1"]);
    expect(items[0]?.textContent).toContain("Current version");
    expect(items[0]?.textContent).toContain("Threshold 6.0 to 6.2 bar");
    expect(items[0]?.textContent).toContain("Reason: v2 raised 9 false alarms while it was stopped.");

    const compare = within(items[0] as HTMLElement).getByRole("button", { name: "Compare v2 with v3" });
    fireEvent.click(compare);
    const diff = within(versions).getByRole("figure");
    expect(diff.textContent).toContain("Differences between v2 and v3");
    const lines = Array.from(diff.querySelectorAll("pre > span")).map((line) => line.textContent);
    expect(lines).toContain("- if p.all(|v| v < 6.0) {");
    expect(lines).toContain("+ if p.all(|v| v < 6.2) {");
    expect(lines).toContain('+ if point("motor_current").last() < 2.0 { return; }');
    fireEvent.click(compare);
    expect(within(versions).queryByRole("figure")).toBeNull();
  });

  test("a deep link opens the named version with its differences, and an unknown version is ignored", async () => {
    const { router } = await open(`${PRESSURE}?version=2`);
    const versions = screen.getByRole("region", { name: "Version history" });
    expect(within(versions).getByRole("figure").textContent).toContain("Differences between v1 and v2");
    expect(within(versions).getByRole("button", { name: "Compare v1 with v2" }).getAttribute("aria-expanded")).toBe(
      "true",
    );
    fireEvent.click(within(versions).getByRole("button", { name: "Compare v2 with v3" }));
    await waitFor(() => expect(router.state.location.search).toBe("?version=3"));
    cleanup();

    await open(`${PRESSURE}?version=9`);
    expect(within(screen.getByRole("region", { name: "Version history" })).queryByRole("figure")).toBeNull();
  });

  test("the first version shows its script, without a comparison", async () => {
    await open(`${PRESSURE}?version=1`);
    const versions = screen.getByRole("region", { name: "Version history" });
    expect(within(versions).getByRole("figure").textContent).toContain("Script of v1");
    expect(within(versions).getByRole("figure").textContent).toContain('point("outlet_pressure").window(2m)');
  });
});

describe("disabling and enabling", () => {
  test("the Email link opens the confirmation as an editor, and disables nothing until the button", async () => {
    const { server, router } = await open(`${PRESSURE}?action=disable`);
    const dialog = await screen.findByRole("alertdialog", { name: 'Disable "Compressor outlet pressure too low"?' });
    expect(dialog.textContent).toContain(
      "The link only leads to this page; the rule is disabled only when you press the button here.",
    );
    expect(server.callsTo("disable")).toHaveLength(0);
    expect(server.rules.get("rule-pressure")?.status).toBe("active");

    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryAllByRole("alertdialog")).toHaveLength(0));
    expect(router.state.location.search).toBe("");
    expect(server.callsTo("disable")).toHaveLength(0);
  });

  test("confirming from the Email link disables the rule and the page shows it with the reason", async () => {
    const { server } = await open(`${PRESSURE}?action=disable`);
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.change(within(dialog).getByLabelText("Reason (optional)"), { target: { value: "Dips at shift change" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Disable rule" }));
    await waitFor(() => expect(server.rules.get("rule-pressure")?.status).toBe("disabled"));
    const note = await screen.findByText("Disabled by Wang Xiaoming on", { exact: false });
    expect(note.closest("[role=status]")?.textContent).toContain("Reason: Dips at shift change");
    expect(screen.getByRole("button", { name: "Enable rule" })).toBeDefined();
    expect(screen.queryByRole("button", { name: "Disable rule" })).toBeNull();
    expect(screen.getAllByText("Disabled").length).toBeGreaterThan(0);
  });

  test("the disable button of the header opens the same confirmation, without the Email note", async () => {
    await open(PRESSURE);
    fireEvent.click(screen.getByRole("button", { name: "Disable rule" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).queryByText(/came here from the link/)).toBeNull();
  });

  test("a rule that is already disabled does not open the confirmation from the link, and can be enabled", async () => {
    const { server } = await open("/alerts/rules/rule-offline?action=disable");
    expect(screen.queryByRole("alertdialog")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Enable rule" }));
    await waitFor(() => expect(server.rules.get("rule-offline")?.status).toBe("active"));
  });

  test("a viewer with the Email link sees the rule with no dialog and no buttons", async () => {
    const { server } = await open(`${PRESSURE}?action=disable`, { role: "viewer" });
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(screen.queryByRole("button", { name: "Disable rule" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Enable rule" })).toBeNull();
    expect(screen.getByText("Disabling and pausing rules needs the editor role or above.")).toBeDefined();
    expect(screen.getByRole("button", { name: "Explain this rule?" })).toBeDefined();
    expect(server.callsTo("disable")).toHaveLength(0);
  });

  test("an AI-made rule shows as paused while every AI rule is, a member-made one does not", async () => {
    const server = new FakeAlertsServer();
    await open(PRESSURE, {}, server);
    await server.pauseAiRules();
    cleanup();
    await open(PRESSURE, { seed: "none" }, server);
    expect(
      within(screen.getByRole("heading", { level: 1 }).closest("header") as HTMLElement).getByText(
        "Paused (all AI rules)",
      ),
    ).toBeDefined();
    cleanup();
    renderAlerts("/alerts/rules/rule-tower", server, { seed: "none" });
    await screen.findByRole("heading", { level: 1, name: "Cooling tower temperature limit" });
    expect(screen.getAllByText("Member-made: Chen Meiling").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Active").length).toBeGreaterThan(0);
    expect(screen.queryByText("Paused (all AI rules)")).toBeNull();
  });
});

describe("states of the rule page", () => {
  test("a rule that does not exist is a not-found page with a way back", async () => {
    await open("/alerts/rules/nothing");
    expect(screen.getByRole("link", { name: "Back to the rules" }).getAttribute("href")).toBe("/alerts?tab=rules");
  });

  test("a failed load says why and retrying shows the rule", async () => {
    const server = new FakeAlertsServer();
    server.failNext("rule");
    renderAlerts(PRESSURE, server);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The rule can't be loaded");
    fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    await screen.findByRole("heading", { level: 1, name: "Compressor outlet pressure too low" });
  });

  test("a link to a section scrolls to it once the rule has loaded", async () => {
    const scrolled: string[] = [];
    const original = Element.prototype.scrollIntoView;
    Element.prototype.scrollIntoView = function (this: Element) {
      scrolled.push(this.id);
    };
    try {
      await open(`${PRESSURE}#recent`);
      await waitFor(() => expect(scrolled).toContain("recent"));
    } finally {
      Element.prototype.scrollIntoView = original;
    }
  });
});
