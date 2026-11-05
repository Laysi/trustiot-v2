import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { renderAlerts } from "../../test/alerts-render.tsx";
import { FakeAlertsServer, fakeAlertsError, seedAlerts } from "../../test/fake-alerts.ts";

afterEach(() => {
  cleanup();
});

async function open(
  path = "/alerts",
  options: Parameters<typeof renderAlerts>[2] = {},
  server = new FakeAlertsServer(),
) {
  const rendered = renderAlerts(path, server, options);
  await screen.findByRole("heading", { level: 1, name: "Alerts and notifications" });
  return { server, ...rendered };
}

function listItems(): HTMLElement[] {
  return within(screen.getByRole("list", { name: "Notifications" })).getAllByRole("listitem");
}

function itemWith(text: string | RegExp): HTMLElement {
  const item = listItems().find((element) =>
    typeof text === "string" ? element.textContent?.includes(text) : text.test(element.textContent ?? ""),
  );
  if (!item) throw new Error(`no notification with ${String(text)}`);
  return item;
}

describe("the notification list", () => {
  test("lists the unread notifications with who made the rule, the merge count and the hourly-limit note", async () => {
    await open();
    await screen.findByRole("list", { name: "Notifications" });

    const pressure = itemWith("Compressor A2 outlet pressure 5.8 bar");
    expect(within(pressure).getByText("AI-made")).toBeDefined();
    expect(within(pressure).getByText("3 triggers merged")).toBeDefined();
    expect(within(pressure).getByText("Unread")).toBeDefined();
    expect(within(pressure).getByRole("link", { name: "Compressor outlet pressure too low" })).toBeDefined();

    expect(within(itemWith("Cooling tower")).getByText("Member-made")).toBeDefined();
    // The limit note says what happened and is not mistaken for a broken rule.
    const note = screen.getByRole("note");
    expect(note.textContent).toContain("has notified 4 times this hour, which is its limit");
    // Read notifications are not in the default (unread) view.
    expect(screen.queryByText("Injection machine 3 has been offline for over 30 minutes")).toBeNull();
    // The tab carries the unread count, the rules tab the number of rules.
    expect(screen.getByRole("tab", { name: /Notifications.*3 unread/ })).toBeDefined();
    expect(screen.getByRole("tab", { name: /Rules.*6 rules/ })).toBeDefined();
  });

  test("shows everything on request, a disabled rule's old notification keeps its text without a disable button", async () => {
    await open();
    await screen.findByRole("list", { name: "Notifications" });
    fireEvent.click(screen.getByRole("button", { name: "All" }));
    const offline = await waitFor(() => itemWith("Injection machine 3 has been offline"));
    expect(within(offline).getByText("Rule disabled")).toBeDefined();
    expect(within(offline).queryByRole("button", { name: "Disable rule" })).toBeNull();
    expect(within(offline).getByRole("link", { name: "Why is it offline?" })).toBeDefined();
  });

  test("an Ask AI button leads to the conversation home with the fixed question and its context", async () => {
    const { router } = await open();
    const item = await waitFor(() => itemWith("Cooling tower"));
    fireEvent.click(within(item).getByRole("link", { name: "Why did it fire?" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    const state = router.state.location.state as { ask: { question: string; context: string[] } };
    expect(state.ask.question).toBe("Why did it fire?");
    expect(state.ask.context[0]).toBe("Rule: Cooling tower temperature limit");
    expect(state.ask.context[1]).toBe("Device: Tower thermometer TT-1");
  });

  test("opening a notification marks it read and leads to the rule's recent triggers; read is the caller's", async () => {
    const { router, server } = await open();
    const item = await waitFor(() => itemWith("Cooling tower"));
    fireEvent.click(within(item).getByRole("link", { name: /Cooling tower outlet temperature 35.4/ }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/alerts/rules/rule-tower"));
    expect(router.state.location.hash).toBe("#recent");
    await waitFor(() => expect(server.callsTo("markRead")).toHaveLength(1));
    expect(server.feed.find((entry) => entry.type === "notification" && entry.rule_id === "rule-tower")).toMatchObject({
      read: true,
    });
  });

  test("marks every notification as read", async () => {
    const { server } = await open();
    await screen.findByRole("list", { name: "Notifications" });
    fireEvent.click(screen.getByRole("button", { name: "Mark all as read" }));
    await waitFor(() => expect(server.callsTo("markAllRead")).toHaveLength(1));
    await screen.findByText("No unread notifications");
    expect(screen.queryByRole("button", { name: "Mark all as read" })).toBeNull();
  });

  test("filters by device and rule, and says when nothing fits", async () => {
    const { router } = await open("/alerts?show=all");
    await screen.findByRole("list", { name: "Notifications" });
    fireEvent.change(screen.getByLabelText("Device"), { target: { value: "dev-tt1" } });
    await waitFor(() => expect(listItems()).toHaveLength(1));
    expect(router.state.location.search).toContain("device=dev-tt1");
    fireEvent.change(screen.getByLabelText("Rule"), { target: { value: "rule-pressure" } });
    await screen.findByText("No notification matches");
    fireEvent.click(screen.getByRole("button", { name: "Clear the filters" }));
    await waitFor(() => expect(listItems().length).toBeGreaterThan(1));
  });

  test("a deep link to one notification shows all of them and marks that one", async () => {
    const server = seedAlerts(new FakeAlertsServer());
    const target = server.feed.find((entry) => entry.type === "notification" && entry.read);
    if (!target) throw new Error("the seed has a read notification");
    renderAlerts(`/alerts/notifications/${target.id}`, server, { seed: "none" });
    await waitFor(() => {
      expect(itemWith("Injection machine 3 has been offline").getAttribute("aria-current")).toBe("true");
    });
    // The unread ones are in the list too, because the link shows all.
    expect(itemWith("Cooling tower").getAttribute("aria-current")).toBeNull();
  });
});

describe("disabling a rule from its notification", () => {
  test("asks for confirmation with an optional reason, then the rule is disabled and the notification says so", async () => {
    const { server } = await open();
    const item = await waitFor(() => itemWith("Cooling tower"));
    fireEvent.click(within(item).getByRole("button", { name: "Disable rule" }));

    const dialog = await screen.findByRole("alertdialog", { name: 'Disable "Cooling tower temperature limit"?' });
    // Nothing is disabled until the button in the dialog (the focus starts on Cancel).
    expect(server.callsTo("disable")).toHaveLength(0);
    expect(within(dialog).queryByText(/came here from the link/)).toBeNull();
    const cancel = within(dialog).getByRole("button", { name: "Cancel" });
    // Compared as booleans: a failing `toBe` on two DOM nodes prints both trees, which is enormous.
    await waitFor(() => expect(document.activeElement === cancel).toBe(true));
    fireEvent.change(within(dialog).getByLabelText("Reason (optional)"), { target: { value: " Tower cleaning " } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Disable rule" }));

    await waitFor(() => expect(screen.queryAllByRole("alertdialog")).toHaveLength(0));
    expect(server.callsTo("disable")[0]?.args).toEqual(["rule-tower", "Tower cleaning"]);
    const after = await waitFor(() => {
      const updated = itemWith("Cooling tower");
      expect(within(updated).getByText("Rule disabled")).toBeDefined();
      return updated;
    });
    expect(within(after).queryByRole("button", { name: "Disable rule" })).toBeNull();
    // The rules tab shows who disabled it.
    fireEvent.click(screen.getByRole("tab", { name: /Rules/ }));
    const row = await screen.findByRole("row", { name: /Cooling tower temperature limit/ });
    expect(within(row).getByText("Disabled")).toBeDefined();
    expect(within(row).getByText(/Wang Xiaoming/)).toBeDefined();
  });

  test("a refusal stays in the dialog, which keeps its text", async () => {
    const { server } = await open();
    const item = await waitFor(() => itemWith("Cooling tower"));
    server.failNext("disable", fakeAlertsError(403, "permission_denied"));
    fireEvent.click(within(item).getByRole("button", { name: "Disable rule" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.change(within(dialog).getByLabelText("Reason (optional)"), { target: { value: "keep me" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Disable rule" }));
    await within(dialog).findByText("You don't have permission to do this.");
    expect((within(dialog).getByLabelText("Reason (optional)") as HTMLTextAreaElement).value).toBe("keep me");
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryAllByRole("alertdialog")).toHaveLength(0));
  });
});

describe("pausing all AI rules", () => {
  test("confirms what stops and what keeps running, then shows the pause and every AI rule as paused", async () => {
    const { server } = await open();
    expect(await screen.findByText("AI rules running: 3.")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Pause all AI rules" }));

    const dialog = await screen.findByRole("alertdialog", { name: "Pause all AI rules?" });
    expect(dialog.textContent).toContain("AI-made rules (3) stop judging and notifying at once");
    expect(dialog.textContent).toContain("Member-made rules (1) keep running");
    expect(server.callsTo("pause")).toHaveLength(0);
    fireEvent.click(within(dialog).getByRole("button", { name: "Pause all AI rules" }));

    const paused = await screen.findByText("All AI rules are paused");
    expect(paused.closest("[role=status]")?.textContent).toContain("Wang Xiaoming paused them on");
    expect(paused.closest("[role=status]")?.textContent).toContain("member-made rules (1) keep running");
    expect(screen.queryByRole("button", { name: "Pause all AI rules" })).toBeNull();

    fireEvent.click(screen.getByRole("tab", { name: /Rules/ }));
    const pressure = await screen.findByRole("row", { name: /Compressor outlet pressure too low/ });
    expect(within(pressure).getByText("Paused (all AI rules)")).toBeDefined();
    // A member-made rule keeps running, and a rule a person disabled stays disabled, apart from the pause.
    expect(
      within(screen.getByRole("row", { name: /Cooling tower temperature limit/ })).getByText("Active"),
    ).toBeDefined();
    expect(within(screen.getByRole("row", { name: /Injection machine offline/ })).getByText("Disabled")).toBeDefined();
  });

  test("resumes without a confirmation", async () => {
    const server = new FakeAlertsServer();
    await open("/alerts", {}, server);
    await server.pauseAiRules();
    await act(async () => {
      await Promise.resolve();
    });
    cleanup();
    await open("/alerts", { seed: "none" }, server);
    const resume = await screen.findByRole("button", { name: "Resume AI rules" });
    fireEvent.click(resume);
    await screen.findByText("AI rules running: 3.");
    expect(server.callsTo("resume")).toHaveLength(1);
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  test("a failed pause says so in the dialog and changes nothing", async () => {
    const { server } = await open();
    await screen.findByText("AI rules running: 3.");
    server.failNext("pause");
    fireEvent.click(screen.getByRole("button", { name: "Pause all AI rules" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Pause all AI rules" }));
    await within(dialog).findByText("The service is temporarily unavailable. Try again later.");
    expect(server.pause.paused).toBe(false);
  });
});

describe("the rule list", () => {
  test("lists each rule with who made it, its state, where it applies and its latest trigger", async () => {
    await open("/alerts?tab=rules");
    const table = await screen.findByRole("table");
    const pressure = within(table).getByRole("row", { name: /Compressor outlet pressure too low/ });
    expect(within(pressure).getByText("AI-made")).toBeDefined();
    expect(within(pressure).getByText("Active")).toBeDefined();
    expect(within(pressure).getByText("Compressor A1, Compressor A2")).toBeDefined();
    expect(within(pressure).getByText("v3")).toBeDefined();
    const failed = within(table).getByRole("row", { name: /Feeder vibration spike/ });
    expect(within(failed).getByText("Backtest failed")).toBeDefined();
    expect(within(failed).getByText("The case that should fire did not")).toBeDefined();
    expect(within(failed).getByText("—")).toBeDefined();
    expect(
      within(within(table).getByRole("row", { name: /Cooling tower/ })).getByText("Member-made: Chen Meiling"),
    ).toBeDefined();
    const disabled = within(table).getByRole("row", { name: /Injection machine offline/ });
    expect(within(disabled).getByText(/Wang Xiaoming/)).toBeDefined();
  });

  test("filters by creator, state and search, in the address", async () => {
    const { router } = await open("/alerts?tab=rules");
    await screen.findByRole("table");
    fireEvent.click(screen.getByRole("button", { name: "Member-made" }));
    await waitFor(() => expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2));
    expect(router.state.location.search).toContain("origin=member");
    fireEvent.click(screen.getByRole("button", { name: "All" }));
    fireEvent.change(screen.getByLabelText("State"), { target: { value: "backtest_failed" } });
    await waitFor(() => expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(2));
    fireEvent.change(screen.getByLabelText("State"), { target: { value: "" } });
    fireEvent.change(screen.getByLabelText("Search rules or devices"), { target: { value: "nothing like this" } });
    await screen.findByText("No rule matches");
  });

  test("an editor disables and enables a rule from its menu", async () => {
    const { server } = await open("/alerts?tab=rules");
    const table = await screen.findByRole("table");
    const row = within(table).getByRole("row", { name: /Injection machine offline/ });
    fireEvent.keyDown(within(row).getByRole("button", { name: "Actions for Injection machine offline" }), {
      key: "Enter",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: "Enable rule" }));
    await waitFor(() => expect(server.callsTo("enable")).toHaveLength(1));
    await waitFor(() =>
      expect(within(screen.getByRole("row", { name: /Injection machine offline/ })).getByText("Active")).toBeDefined(),
    );

    const standby = screen.getByRole("row", { name: /Press standby power too high/ });
    fireEvent.keyDown(within(standby).getByRole("button", { name: "Actions for Press standby power too high" }), {
      key: "Enter",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: "Disable rule" }));
    const dialog = await screen.findByRole("alertdialog", { name: 'Disable "Press standby power too high"?' });
    fireEvent.click(within(dialog).getByRole("button", { name: "Disable rule" }));
    await waitFor(() => expect(server.rules.get("rule-standby")?.status).toBe("disabled"));
  });

  test("a failed enable is reported on the page", async () => {
    const { server } = await open("/alerts?tab=rules");
    const row = await screen.findByRole("row", { name: /Injection machine offline/ });
    server.failNext("enable");
    fireEvent.keyDown(within(row).getByRole("button", { name: "Actions for Injection machine offline" }), {
      key: "Enter",
    });
    fireEvent.click(await screen.findByRole("menuitem", { name: "Enable rule" }));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("temporarily unavailable");
    fireEvent.click(within(alert).getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("alert")).toBeNull();
  });

  test("the tabs follow the arrow keys and the address", async () => {
    const { router } = await open();
    const notifications = await screen.findByRole("tab", { name: /Notifications/ });
    expect(notifications.getAttribute("aria-selected")).toBe("true");
    notifications.focus();
    fireEvent.keyDown(notifications, { key: "ArrowRight" });
    await screen.findByRole("table");
    expect(router.state.location.search).toBe("?tab=rules");
    expect(document.activeElement === screen.getByRole("tab", { name: /Rules/ })).toBe(true);
    expect(screen.getByRole("tab", { name: /Notifications/ }).getAttribute("tabindex")).toBe("-1");
  });
});

describe("states of the page", () => {
  test("a tenant without notifications is told what the page is for and what to do first", async () => {
    await open("/alerts", { seed: "empty" });
    const empty = await screen.findByText("No notifications yet");
    expect(empty.closest("[role=status]")?.textContent).toContain("A newly connected device usually needs a few days");
    // In the header and in the empty state.
    expect(screen.getAllByRole("link", { name: "Ask AI to suggest alert rules" })).toHaveLength(2);
    // With no AI rules there is nothing to pause.
    expect(screen.queryByRole("button", { name: "Pause all AI rules" })).toBeNull();
    expect(screen.queryByText(/AI rules running/)).toBeNull();
    fireEvent.click(screen.getByRole("tab", { name: /Rules/ }));
    await screen.findByText("No rules yet");
  });

  test("loading shows a skeleton announced once", async () => {
    const server = new FakeAlertsServer();
    server.hold("overview");
    renderAlerts("/alerts", server);
    expect((await screen.findByText("Loading notifications…")).getAttribute("role")).toBe("status");
    act(() => server.release("overview"));
    await screen.findByRole("tab", { name: /Notifications/ });
  });

  test("a failed load says rules keep running, and retrying shows the page", async () => {
    const server = new FakeAlertsServer();
    server.failNext("overview");
    renderAlerts("/alerts", server);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Alerts can't be loaded right now");
    expect(alert.textContent).toContain("Rules keep running, and email and webhooks are still sent");
    fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    await screen.findByRole("tab", { name: /Notifications/ });
  });

  test("a failed list keeps the page and its tabs", async () => {
    const server = new FakeAlertsServer();
    server.failNext("notifications");
    renderAlerts("/alerts", server);
    await screen.findByText("Alerts can't be loaded right now");
    expect(screen.getByRole("tab", { name: /Rules/ })).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByRole("list", { name: "Notifications" });
  });

  test("a plan without alerts shows the upgrade wall, with the next step for the role", async () => {
    const server = new FakeAlertsServer();
    server.planAllows = false;
    await open("/alerts", { role: "admin" }, server);
    expect(await screen.findByText("Alerts are a Business plan feature")).toBeDefined();
    expect(screen.getByText(/you are one/)).toBeDefined();
    expect(screen.getByRole("link", { name: "Ask AI what looks unusual first" })).toBeDefined();
    expect(screen.queryByRole("tab")).toBeNull();
    cleanup();
    await open("/alerts", { role: "editor" }, server);
    expect(await screen.findByText("Only organization admins can upgrade; ask one of them.")).toBeDefined();
  });

  test("an address that names no notification is a not-found page with a way back", async () => {
    const server = new FakeAlertsServer();
    renderAlerts("/alerts/notifications/does-not-exist", server);
    expect(await screen.findByRole("heading", { name: "Notification not found" })).toBeDefined();
    expect(screen.getByRole("link", { name: "Back to the notifications" }).getAttribute("href")).toBe("/alerts");
  });
});

describe("a notification's deep link", () => {
  test("the Rules tab leaves the link for the plain page with that tab", async () => {
    const server = seedAlerts(new FakeAlertsServer());
    const target = server.feed.find((entry) => entry.type === "notification");
    if (!target) throw new Error("the seed has notifications");
    const { router } = renderAlerts(`/alerts/notifications/${target.id}`, server, { seed: "none" });
    fireEvent.click(await screen.findByRole("tab", { name: /Rules/ }));
    await screen.findByRole("table");
    expect(router.state.location.pathname).toBe("/alerts");
    expect(router.state.location.search).toBe("?tab=rules");
  });

  test("a failed lookup is shown with a retry, and the list stays", async () => {
    const server = seedAlerts(new FakeAlertsServer());
    const target = server.feed.find((entry) => entry.type === "notification");
    if (!target) throw new Error("the seed has notifications");
    server.failNext("notification");
    renderAlerts(`/alerts/notifications/${target.id}`, server, { seed: "none" });
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The notification this link names can't be loaded");
    await screen.findByRole("list", { name: "Notifications" });
    fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(screen.queryAllByRole("alert")).toHaveLength(0));
  });
});

describe("a viewer", () => {
  test("reads everything and asks the assistant, with no disable, enable or pause button", async () => {
    await open("/alerts", { role: "viewer" });
    const item = await waitFor(() => itemWith("Cooling tower"));
    expect(within(item).getByRole("link", { name: "Why did it fire?" })).toBeDefined();
    expect(within(item).queryByRole("button", { name: "Disable rule" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Pause all AI rules" })).toBeNull();
    expect(screen.getByText(/needs the editor role or above/)).toBeDefined();
    fireEvent.click(screen.getByRole("tab", { name: /Rules/ }));
    await screen.findByRole("table");
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
  });

  test("sees the pause but cannot resume it", async () => {
    const server = new FakeAlertsServer();
    await open("/alerts", { role: "viewer" }, server);
    await server.pauseAiRules();
    cleanup();
    await open("/alerts", { role: "viewer", seed: "none" }, server);
    await screen.findByText("All AI rules are paused");
    expect(screen.queryByRole("button", { name: "Resume AI rules" })).toBeNull();
  });
});
