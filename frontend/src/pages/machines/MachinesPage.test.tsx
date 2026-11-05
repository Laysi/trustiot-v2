import { afterEach, beforeEach, describe, expect, setSystemTime, test } from "bun:test";
import { cleanup, fireEvent, screen, within } from "@testing-library/react";
import { FakeMachineServer, machineError } from "../../test/fake-machines.ts";
import { dialogClosed, NOW, openMachines, plant, press, until } from "./test-support.tsx";

beforeEach(() => {
  setSystemTime(new Date(NOW));
});

afterEach(() => {
  cleanup();
  setSystemTime();
});

async function open(machines: FakeMachineServer, options: Parameters<typeof openMachines>[2] = {}, path = "/machines") {
  const rendered = openMachines(machines, path, options);
  await screen.findByRole("heading", { level: 1, name: "Machines" });
  return rendered;
}

function card(name: string): HTMLElement {
  return screen.getByRole("article", { name });
}

describe("the machine list", () => {
  test("shows each machine's state, part, start and devices; a utility has no production buttons", async () => {
    const { machines, devices } = plant();
    await open(machines, { devices });
    const press = await screen.findByRole("article", { name: "Press A" });
    expect(within(press).getByText("In production")).toBeTruthy();
    expect(within(press).getByText("BRK-1023")).toBeTruthy();
    expect(within(press).getByText(/Started 00:12 · 6 h 19 min/)).toBeTruthy();
    expect(within(press).getByText(/Work order WO-5531/)).toBeTruthy();
    expect(within(press).getByText("meter-1, vibration-3")).toBeTruthy();
    expect(within(press).getByRole("button", { name: "Change over, Press A" })).toBeTruthy();
    expect(within(press).getByRole("button", { name: "End production, Press A" })).toBeTruthy();

    const idle = card("Press B");
    expect(within(idle).getByText("Not producing")).toBeTruthy();
    expect(within(idle).getByText("CAP-220")).toBeTruthy();
    expect(within(idle).getByRole("button", { name: "Start production, Press B" })).toBeTruthy();

    const utility = card("Compressor A1");
    expect(within(utility).getByText("Utility")).toBeTruthy();
    expect(within(utility).queryByRole("button")).toBeNull();
  });

  test("a viewer sees the machines and no buttons that change anything", async () => {
    const { machines } = plant();
    await open(machines, { role: "viewer" });
    await screen.findByRole("article", { name: "Press A" });
    expect(screen.queryByRole("button", { name: /Change over|Start production|End production/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Add machine" })).toBeNull();
  });

  test("a changeover on the card notes the server's time, ends the old part, and offers a correction", async () => {
    const { machines } = plant();
    await open(machines);
    fireEvent.click(await screen.findByRole("button", { name: "Change over, Press A" }));
    const dialog = await screen.findByRole("dialog", { name: "Change over · Press A" });
    // No start time to fill in (decision Q10): the system notes it.
    expect(dialog.querySelector("input[type=datetime-local]")).toBeNull();
    expect(within(dialog).getByText(/ends the moment you press Confirm/)).toBeTruthy();
    expect(within(dialog).getByText(/The system notes the start time \(now 06:32\)/)).toBeTruthy();
    // The part number has the focus, and the recently used ones are one tap away.
    expect(document.activeElement).toBe(within(dialog).getByLabelText("New part number"));
    fireEvent.click(await within(dialog).findByRole("button", { name: "BRK-1100" }));
    fireEvent.change(within(dialog).getByLabelText("Work order (optional)"), { target: { value: "WO-5600" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm changeover" }));

    await screen.findByText("Changed over: BRK-1023 → BRK-1100, started 06:32.");
    expect(screen.queryByRole("dialog")).toBeNull();
    const press = card("Press A");
    expect(within(press).getByText("BRK-1100")).toBeTruthy();
    expect(within(press).getByText(/Work order WO-5600/)).toBeTruthy();
    const ended = [...machines.runs.values()].find((run) => run.part_number === "BRK-1023");
    expect(ended?.ended_at).toBe(new Date(NOW).toISOString());
    expect(screen.getByRole("link", { name: "Correct the record, Press A" })).toBeTruthy();
  });

  test("the part number is required, and a target quantity must be a whole number", async () => {
    const { machines } = plant();
    await open(machines);
    fireEvent.click(await screen.findByRole("button", { name: "Change over, Press A" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("Target quantity (optional)"), { target: { value: "12.5" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Confirm changeover" }));
    expect(await within(dialog).findByText("Enter the part number.")).toBeTruthy();
    expect(within(dialog).getByText(/whole number above zero/)).toBeTruthy();
    expect(machines.calls.some((call) => call.method === "changeover")).toBe(false);
  });

  test("starting production on an idle machine says nothing is in progress", async () => {
    const { machines } = plant();
    await open(machines);
    fireEvent.click(await screen.findByRole("button", { name: "Start production, Press B" }));
    const dialog = await screen.findByRole("dialog", { name: "Start production · Press B" });
    expect(within(dialog).getByText(/Nothing is in progress on this machine now/)).toBeTruthy();
    fireEvent.change(within(dialog).getByLabelText("New part number"), { target: { value: "CAP-300" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Start production" }));
    await screen.findByText("Production started: CAP-300, started 06:32.");
    expect(within(card("Press B")).getByText("In production")).toBeTruthy();
  });

  test("ending production asks first, and a refusal stays in the dialog", async () => {
    const { machines } = plant();
    await open(machines);
    fireEvent.click(await screen.findByRole("button", { name: "End production, Press A" }));
    const dialog = await screen.findByRole("alertdialog", { name: "End production on Press A?" });
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    machines.failNext("endRun", machineError(409, "conflict", "field_context.no_run_in_progress"));
    fireEvent.click(within(dialog).getByRole("button", { name: "End production" }));
    expect(await within(dialog).findByRole("alert")).toBeTruthy();
    await press(within(dialog).getByRole("button", { name: "End production" }));
    await dialogClosed();
    expect(within(card("Press A")).getByText("Not producing")).toBeTruthy();
  });

  test("search and the in-production filter are kept in the address", async () => {
    const { machines } = plant();
    const { router } = await open(machines);
    await screen.findByRole("article", { name: "Press B" });
    fireEvent.click(screen.getByRole("button", { name: "In production 1" }));
    expect(router.state.location.search).toBe("?show=running");
    expect(screen.queryByRole("article", { name: "Press B" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /^All/ }));
    fireEvent.change(screen.getByRole("searchbox", { name: "Search machines or part numbers" }), {
      target: { value: "cap-220" },
    });
    expect(router.state.location.search).toBe("?q=cap-220");
    expect(screen.getByRole("article", { name: "Press B" })).toBeTruthy();
    expect(screen.queryByRole("article", { name: "Press A" })).toBeNull();
    fireEvent.change(screen.getByRole("searchbox", { name: "Search machines or part numbers" }), {
      target: { value: "nothing like this" },
    });
    expect(screen.getByText("No machine matches")).toBeTruthy();
  });

  test("an empty tenant is told what machines are for; an editor can add the first one", async () => {
    const machines = new FakeMachineServer();
    const { router } = await open(machines);
    expect(await screen.findByText("Add your first machine")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Add machine" }));
    const dialog = await screen.findByRole("dialog", { name: "Add machine" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await within(dialog).findByText("Enter a name.")).toBeTruthy();
    fireEvent.change(within(dialog).getByLabelText("Name"), { target: { value: "Press C" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "Save" }));
    await until("the new machine's page", () => /^\/machines\/.+/.test(router.state.location.pathname));
    expect(await screen.findByRole("heading", { level: 1, name: "Press C" })).toBeTruthy();
    expect(screen.getByText("Machine Press C added.")).toBeTruthy();
  });

  test("a viewer of an empty tenant is pointed to someone who can add machines", async () => {
    await open(new FakeMachineServer(), { role: "viewer" });
    expect(await screen.findByText("Ask a member with edit permission to add one.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Add machine" })).toBeNull();
  });

  test("while the list loads it says so once", async () => {
    const { machines } = plant();
    const release = machines.holdNext("listMachines");
    openMachines(machines, "/machines");
    expect(await screen.findByText("Loading machines…")).toBeTruthy();
    release();
    await screen.findByRole("article", { name: "Press A" });
  });

  test("a service that does not answer shows what to do meanwhile, with a retry", async () => {
    const { machines } = plant();
    machines.unavailable = true;
    await open(machines);
    expect(await screen.findByText("Production records can't be saved right now")).toBeTruthy();
    expect(screen.getByText(/add them as a record when it is back/)).toBeTruthy();
    machines.unavailable = false;
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByRole("article", { name: "Press A" });
  });

  test("another failure shows its message", async () => {
    const { machines } = plant();
    machines.failNext("listMachines", machineError(500, "internal", "error.internal"));
    await open(machines);
    expect(await screen.findByRole("alert")).toBeTruthy();
  });

  test("a Starter tenant gets the upgrade wall instead of the list", async () => {
    const { machines } = plant();
    machines.plan = "starter";
    await open(machines);
    expect(await screen.findByText("Machines and production records come with Business and above")).toBeTruthy();
    expect(screen.queryByRole("article")).toBeNull();
  });
});
