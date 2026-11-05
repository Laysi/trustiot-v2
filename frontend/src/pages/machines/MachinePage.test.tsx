import { afterEach, beforeEach, describe, expect, setSystemTime, test } from "bun:test";
import { act, cleanup, fireEvent, screen, within } from "@testing-library/react";

import { FakeMachineServer, machineError } from "../../test/fake-machines.ts";
import { dialogClosed, NOW, openMachines, plant, press, until } from "./test-support.tsx";

beforeEach(() => {
  setSystemTime(new Date(NOW));
});

afterEach(() => {
  cleanup();
  setSystemTime();
});

type Plant = ReturnType<typeof plant>;

async function openPress(
  fixture: Plant,
  options: { role?: "viewer" | "editor" | "admin"; query?: string; machineId?: string } = {},
) {
  const id = options.machineId ?? fixture.pressA.machine_id;
  const rendered = openMachines(fixture.machines, `/machines/${id}${options.query ?? ""}`, {
    ...(options.role ? { role: options.role } : {}),
    devices: fixture.devices,
  });
  await screen.findByRole("heading", { level: 1 });
  return rendered;
}

function runsTable(): HTMLElement {
  return screen.getByRole("table", { name: "Production records, newest first" });
}

describe("a machine's page", () => {
  test("shows what is in production, the records of the week with their marks, the change record, points and relations", async () => {
    const fixture = plant();
    await openPress(fixture);
    expect(screen.getByRole("heading", { level: 1, name: "Press A" })).toBeTruthy();
    const now = screen.getByRole("region", { name: "Now in production" });
    expect(within(now).getByText("BRK-1023")).toBeTruthy();
    expect(within(now).getByText("Started 00:12 (system record)")).toBeTruthy();
    expect(within(now).getByText("Work order WO-5531")).toBeTruthy();
    expect(within(now).getByText("Target 1,200 pieces")).toBeTruthy();
    expect(within(now).getByRole("button", { name: "Change over" })).toBeTruthy();
    expect(within(now).getByRole("button", { name: "End production, Press A" })).toBeTruthy();

    const rows = within(await screen.findByRole("table", { name: "Production records, newest first" })).getAllByRole(
      "row",
    );
    expect(rows).toHaveLength(4);
    expect(within(rows[1] as HTMLElement).getByText("In progress")).toBeTruthy();
    expect(within(rows[2] as HTMLElement).getByText("Corrected 1 times")).toBeTruthy();
    expect(within(rows[3] as HTMLElement).getByText("Added later")).toBeTruthy();

    const changes = screen.getByRole("region", { name: "Change record" });
    expect(await within(changes).findByText("BRK-0988 → BRK-1023")).toBeTruthy();
    expect(within(changes).getByText(/start Yesterday 09:02 → Yesterday 08:32/)).toBeTruthy();
    expect(within(changes).getByText(/Reason: forgot to press at the shift change/)).toBeTruthy();

    const points = screen.getByRole("region", { name: "Devices and data points" });
    expect(await within(points).findByRole("link", { name: "meter-1" })).toBeTruthy();
    expect(within(points).getByText("All data points (2)")).toBeTruthy();
    expect(within(points).getByText("rms, peak")).toBeTruthy();

    const relations = screen.getByRole("region", { name: "Relations" });
    expect(within(relations).getByRole("link", { name: "Compressor A1" })).toBeTruthy();
    expect(within(relations).getByText("powers this machine")).toBeTruthy();
    expect(within(relations).getByRole("link", { name: "Feeder F2" })).toBeTruthy();
    expect(within(relations).getByText("Upstream")).toBeTruthy();
  });

  test("a viewer reads the page and sees no button that changes anything", async () => {
    const fixture = plant();
    await openPress(fixture, { role: "viewer" });
    await screen.findByRole("table", { name: "Production records, newest first" });
    for (const name of [
      /Change over/,
      /End production/,
      "Edit",
      "Add a record",
      "Manage",
      /^Correct/,
      /More actions/,
    ]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    expect(screen.queryByRole("button", { name: "Edit" })).toBeNull();
  });

  test("a utility has no production block", async () => {
    const fixture = plant();
    await openPress(fixture, { machineId: fixture.compressor.machine_id });
    expect(screen.queryByRole("region", { name: "Now in production" })).toBeNull();
    expect(screen.getByText("Utility")).toBeTruthy();
  });

  test("?run= marks and names the record; one outside the week is said to be missing", async () => {
    const fixture = plant();
    await openPress(fixture, { query: `?run=${fixture.previous.run_id}` });
    await screen.findByRole("table", { name: "Production records, newest first" });
    const marked = runsTable().querySelector(`#run-${fixture.previous.run_id}`);
    expect(marked?.getAttribute("aria-current")).toBe("true");
    cleanup();
    await openPress(fixture, { query: "?run=00000000-0000-4000-8000-0000000000ff" });
    expect(await screen.findByText(/isn't among the last 7 days/)).toBeTruthy();
  });

  test("a changeover ends the current record, starts the new one at the server's time and offers a correction", async () => {
    const fixture = plant();
    await openPress(fixture);
    fireEvent.click(await screen.findByRole("button", { name: "Change over" }));
    const dialog = await screen.findByRole("dialog", { name: "Change over · Press A" });
    fireEvent.change(within(dialog).getByLabelText("New part number"), { target: { value: "BRK-1200" } });
    await press(within(dialog).getByRole("button", { name: "Confirm changeover" }));
    await screen.findByText("Changed over: BRK-1023 → BRK-1200, started 06:32.");
    const now = screen.getByRole("region", { name: "Now in production" });
    expect(await within(now).findByText("BRK-1200")).toBeTruthy();
    const changes = screen.getByRole("region", { name: "Change record" });
    expect(await within(changes).findByText("BRK-1023 → BRK-1200")).toBeTruthy();
  });

  test("a backfill that overlaps names the record it ran into and leads to correcting that one", async () => {
    const fixture = plant();
    await openPress(fixture);
    fireEvent.click(await screen.findByRole("button", { name: "Add a record" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a production record · Press A" });
    fireEvent.change(within(dialog).getByLabelText("Part number"), {
      target: { value: "BRK-1300" },
    });
    fireEvent.change(within(dialog).getByLabelText("Start"), { target: { value: "2026-11-03T09:00" } });
    fireEvent.change(within(dialog).getByLabelText("End"), { target: { value: "2026-11-03T15:00" } });
    await press(within(dialog).getByRole("button", { name: "Save" }));
    const alert = await within(dialog).findByRole("alert");
    expect(within(alert).getByText("The time overlaps another record")).toBeTruthy();
    expect(within(alert).getByText(/BRK-0988 \(.*\) already covers part of this time/)).toBeTruthy();
    expect(within(dialog).getByText(/overlaps BRK-0988/)).toBeTruthy();
    expect(within(dialog).getAllByLabelText("Start")[0]?.getAttribute("aria-invalid")).toBe("true");
    // The conflicting record is offered for correction, prefilled.
    await press(within(alert).getByRole("button", { name: "Correct that record" }));
    const correction = await screen.findByRole("dialog", { name: "Correct a production record · Press A" });
    expect((within(correction).getByLabelText("Part number") as HTMLInputElement).value).toBe("BRK-0988");
  });

  test("a backfill inside a gap is saved, marked 'Added later', and logged", async () => {
    const fixture = plant();
    await openPress(fixture);
    fireEvent.click(await screen.findByRole("button", { name: "Add a record" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a production record · Press A" });
    fireEvent.change(within(dialog).getByLabelText("Part number"), { target: { value: "BRK-1300" } });
    fireEvent.change(within(dialog).getByLabelText("Start"), { target: { value: "2026-11-02T08:00" } });
    fireEvent.change(within(dialog).getByLabelText("End"), { target: { value: "2026-11-02T12:00" } });
    // A reason is optional for a backfill (decision Q9).
    await press(within(dialog).getByRole("button", { name: "Save" }));
    await dialogClosed();
    const saved = [...fixture.machines.runs.values()].find((run) => run.part_number === "BRK-1300");
    expect(saved?.source).toBe("backfill");
    expect(saved?.started_at).toBe("2026-11-02T08:00:00.000Z");
    expect(await within(screen.getByRole("region", { name: "Change record" })).findByText(/BRK-1300, /)).toBeTruthy();
  });

  test("a correction needs a reason, keeps the old record's history, and says how often it was corrected", async () => {
    const fixture = plant();
    await openPress(fixture);
    const table = await screen.findByRole("table", { name: "Production records, newest first" });
    fireEvent.click(within(table).getByRole("button", { name: /Correct BRK-1100/ }));
    const dialog = await screen.findByRole("dialog", { name: "Correct a production record · Press A" });
    fireEvent.change(within(dialog).getByLabelText("Part number"), { target: { value: "BRK-1101" } });
    await press(within(dialog).getByRole("button", { name: "Save" }));
    expect(await within(dialog).findByText("Say why the record is corrected.")).toBeTruthy();
    expect(fixture.machines.calls.some((call) => call.method === "correctRun")).toBe(false);
    fireEvent.change(within(dialog).getByLabelText("Reason"), { target: { value: "typo in the part number" } });
    await press(within(dialog).getByRole("button", { name: "Save" }));
    await dialogClosed();
    expect(fixture.machines.runs.get(fixture.backfilled.run_id)?.part_number).toBe("BRK-1101");
    const changes = screen.getByRole("region", { name: "Change record" });
    expect(await within(changes).findByText(/part number BRK-1100 → BRK-1101/)).toBeTruthy();
    expect(within(changes).getByText(/Reason: typo in the part number/)).toBeTruthy();
  });

  test("the record in progress can be corrected without an end", async () => {
    const fixture = plant();
    await openPress(fixture);
    const table = await screen.findByRole("table", { name: "Production records, newest first" });
    fireEvent.click(within(table).getByRole("button", { name: /Correct BRK-1023/ }));
    const dialog = await screen.findByRole("dialog", { name: "Correct a production record · Press A" });
    expect(within(dialog).getByText("This record is still in progress.")).toBeTruthy();
    fireEvent.change(within(dialog).getByLabelText("Start"), { target: { value: "2026-11-04T00:15" } });
    fireEvent.change(within(dialog).getByLabelText("Reason"), { target: { value: "pressed late" } });
    await press(within(dialog).getByRole("button", { name: "Save" }));
    await dialogClosed();
    const run = fixture.machines.runs.get(fixture.current.run_id);
    expect(run?.started_at).toBe("2026-11-04T00:15:00.000Z");
    expect(run?.ended_at).toBeNull();
  });

  test("the points dialog shares a device between machines: a point of another machine is shown with its machine and cannot be taken", async () => {
    const fixture = plant();
    await openPress(fixture);
    fireEvent.click(await screen.findByRole("button", { name: "Manage" }));
    const dialog = await screen.findByRole("dialog", { name: "Devices and data points of Press A" });
    const bearing = await within(dialog).findByLabelText(/bearing/);
    expect((bearing as HTMLInputElement).disabled).toBe(true);
    expect(within(dialog).getByText("Belongs to Feeder F2")).toBeTruthy();
    expect((within(dialog).getByLabelText(/^rms/) as HTMLInputElement).checked).toBe(true);
    fireEvent.click(within(dialog).getByLabelText(/^peak/));
    await press(within(dialog).getByRole("button", { name: "Save" }));
    await dialogClosed();
    expect(
      fixture.machines.assignments
        .filter(
          (item) => item.machine_id === fixture.pressA.machine_id && item.device_id === fixture.vibration.device_id,
        )
        .map((item) => item.point_key),
    ).toEqual(["rms"]);
  });

  test("relations are edited in a dialog", async () => {
    const fixture = plant();
    await openPress(fixture);
    const relations = screen.getByRole("region", { name: "Relations" });
    fireEvent.click(within(relations).getByRole("button", { name: "Edit relations" }));
    const dialog = await screen.findByRole("dialog", { name: "Relations of Press A" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Add a relation" }));
    fireEvent.change(within(dialog).getByLabelText("Relation 3"), { target: { value: "flow_down" } });
    fireEvent.change(within(dialog).getByLabelText("Machine of relation 3"), {
      target: { value: fixture.pressB.machine_id },
    });
    await press(within(dialog).getByRole("button", { name: "Save" }));
    await dialogClosed();
    expect(
      fixture.machines.relations.some(
        (item) =>
          item.kind === "flow" &&
          item.from_machine_id === fixture.pressA.machine_id &&
          item.to_machine_id === fixture.pressB.machine_id,
      ),
    ).toBe(true);
    expect(await within(relations).findByText("Downstream")).toBeTruthy();
  });

  test("deleting a machine asks first and leads back to the list with a message", async () => {
    const fixture = plant();
    const { router } = await openPress(fixture);
    const trigger = await screen.findByRole("button", { name: "More actions for Press A" });
    await act(async () => {
      fireEvent.keyDown(trigger, { key: "Enter" });
    });
    fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Delete machine" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete Press A?" });
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    await press(within(dialog).getByRole("button", { name: "Delete machine" }));
    await until("the list to open", () => router.state.location.pathname === "/machines");
    expect(await screen.findByText("Machine Press A deleted.")).toBeTruthy();
    expect(fixture.machines.machines.has(fixture.pressA.machine_id)).toBe(false);
  });

  test("editing renames the machine", async () => {
    const fixture = plant();
    await openPress(fixture);
    fireEvent.click(await screen.findByRole("button", { name: "Edit" }));
    const dialog = await screen.findByRole("dialog", { name: "Edit machine" });
    fireEvent.change(within(dialog).getByLabelText("Name"), { target: { value: "Press A (line 2)" } });
    await press(within(dialog).getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Press A (line 2)" })).toBeTruthy();
  });

  test("a name that looks like Cypher or SQL is shown as it was typed", async () => {
    const fixture = plant();
    const odd = fixture.machines.addMachine("') RETURN 1 // DROP TABLE machines; --");
    await openPress(fixture, { machineId: odd.machine_id });
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("') RETURN 1 // DROP TABLE machines; --");
  });

  test("a machine that does not exist, and a service that does not answer", async () => {
    const fixture = plant();
    openMachines(fixture.machines, "/machines/00000000-0000-4000-8000-0000000000aa", { devices: fixture.devices });
    expect(await screen.findByRole("heading", { level: 1, name: "Machine not found" })).toBeTruthy();
    cleanup();

    fixture.machines.unavailable = true;
    openMachines(fixture.machines, `/machines/${fixture.pressA.machine_id}`, { devices: fixture.devices });
    expect(await screen.findByText("Production records can't be saved right now")).toBeTruthy();
    expect(screen.getByText(/Alerts keep using the last known part meanwhile/)).toBeTruthy();
    fixture.machines.unavailable = false;
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { level: 1, name: "Press A" })).toBeTruthy();
  });

  test("a failing change says why in the dialog and keeps what was typed", async () => {
    const fixture = plant();
    await openPress(fixture);
    fireEvent.click(await screen.findByRole("button", { name: "Change over" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("New part number"), { target: { value: "BRK-1200" } });
    fixture.machines.failNext("changeover", machineError(503, "unavailable", "error.unavailable"));
    await press(within(dialog).getByRole("button", { name: "Confirm changeover" }));
    expect(await within(dialog).findByText("Production records can't be saved right now")).toBeTruthy();
    expect((within(dialog).getByLabelText("New part number") as HTMLInputElement).value).toBe("BRK-1200");
  });

  test("while the page loads it says so once; an empty week says so", async () => {
    const fixture = plant();
    const fresh = new FakeMachineServer();
    const lonely = fresh.addMachine("Lonely");
    const release = fresh.holdNext("getMachine");
    openMachines(fresh, `/machines/${lonely.machine_id}`, { devices: fixture.devices });
    expect(await screen.findByText("Loading the machine…")).toBeTruthy();
    release();
    expect(await screen.findByText("No production records in the last 7 days.")).toBeTruthy();
    expect(screen.getByText("Nothing is in production.")).toBeTruthy();
  });
});
