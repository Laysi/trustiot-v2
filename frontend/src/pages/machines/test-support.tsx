import { act, fireEvent } from "@testing-library/react";
import { Outlet } from "react-router";

import { MachineApiProvider } from "../../api/pending/MachineApiProvider.tsx";
import { ROUTES } from "../../app/routes.tsx";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { FakeMachineServer } from "../../test/fake-machines.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { withRoutes } from "../../ui-preview/collect.ts";
import { previewRoutes } from "./preview.ts";

/** The instant the tests run at: a Wednesday morning, UTC. */
export const NOW = Date.parse("2026-11-04T06:32:00Z");
export const HOUR = 3_600_000;

/** A fictional plant: a press making BRK-1023, an idle press, an air compressor, and two meters. */
export function plant() {
  const devices = new FakeDeviceServer();
  const meter = devices.addDevice({
    name: "meter-1",
    modelName: "power-meter",
    points: [
      { key: "voltage", data_type: "f32" },
      { key: "power", data_type: "f32" },
    ],
  });
  const vibration = devices.addDevice({
    name: "vibration-3",
    modelName: "vibration",
    points: [
      { key: "rms", data_type: "f32" },
      { key: "peak", data_type: "f32" },
      { key: "bearing", data_type: "f32" },
    ],
  });
  const machines = new FakeMachineServer();
  const pressA = machines.addMachine("Press A", { description: "200 t press, line 2" });
  const pressB = machines.addMachine("Press B");
  const compressor = machines.addMachine("Compressor A1", { utility: true });
  const feeder = machines.addMachine("Feeder F2");
  const current = machines.addRun(pressA.machine_id, "BRK-1023", NOW - 6.33 * HOUR, null, {
    workOrder: "WO-5531",
    targetQuantity: 1200,
  });
  const previous = machines.addRun(pressA.machine_id, "BRK-0988", NOW - 22 * HOUR, NOW - 6.33 * HOUR, {
    workOrder: "WO-5520",
    revisions: 1,
  });
  const backfilled = machines.addRun(pressA.machine_id, "BRK-1100", NOW - 3 * 24 * HOUR, NOW - 2.5 * 24 * HOUR, {
    source: "backfill",
  });
  machines.addRun(pressB.machine_id, "CAP-220", NOW - 30 * HOUR, NOW - 12 * HOUR);
  machines.addChange(pressA.machine_id, {
    kind: "changeover",
    run_id: current.run_id,
    actor: "Wang",
    at: new Date(NOW - 6.33 * HOUR).toISOString(),
    part_number: "BRK-1023",
    previous_part_number: "BRK-0988",
    started_at: current.started_at,
    ended_at: null,
    changes: [],
    reason: null,
  });
  machines.addChange(pressA.machine_id, {
    kind: "correction",
    run_id: previous.run_id,
    actor: "Chen",
    at: new Date(NOW - 20 * HOUR).toISOString(),
    part_number: "BRK-0988",
    previous_part_number: null,
    started_at: previous.started_at,
    ended_at: previous.ended_at,
    changes: [{ field: "started_at", before: new Date(NOW - 21.5 * HOUR).toISOString(), after: previous.started_at }],
    reason: "forgot to press at the shift change",
  });
  for (const key of ["voltage", "power"]) machines.assign(pressA.machine_id, meter.device_id, key);
  machines.assign(pressA.machine_id, vibration.device_id, "rms");
  machines.assign(pressA.machine_id, vibration.device_id, "peak");
  machines.assign(feeder.machine_id, vibration.device_id, "bearing");
  machines.relate("power", compressor.machine_id, pressA.machine_id);
  machines.relate("flow", feeder.machine_id, pressA.machine_id);
  return { devices, machines, pressA, pressB, compressor, feeder, meter, vibration, current, previous, backfilled };
}

type Role = "viewer" | "editor" | "admin";

/** Opens `path` of the machine pages as `role` (an editor unless said), against `machines`. */
export function openMachines(
  machines: FakeMachineServer,
  path: string,
  options: { role?: Role; devices?: FakeDeviceServer } = {},
) {
  const routes = withRoutes(ROUTES, [
    {
      element: (
        <MachineApiProvider api={machines}>
          <Outlet />
        </MachineApiProvider>
      ),
      children: previewRoutes,
    },
  ]);
  return renderAt(path, {
    routes,
    identity: signedInBrowser({ role: options.role ?? "editor" }).browser,
    devices: (options.devices ?? new FakeDeviceServer()).api(),
  });
}

/**
 * Clicks `element` and lets what the click starts (a request, the re-render after it) finish
 * before the test goes on: a `waitFor` that starts in the middle of that stalls for seconds in
 * happy-dom, while one that starts after it finds the answer at once.
 */
export async function press(element: HTMLElement): Promise<void> {
  await act(async () => {
    fireEvent.click(element);
  });
}

/**
 * Waits until `check` holds, and fails naming `what` it waited for. It polls with plain timers
 * inside `act` instead of using Testing Library's `waitFor`: with a Radix modal open or closing,
 * `waitFor` in happy-dom stalls (and has crashed bun), while this finds the answer at once.
 */
export async function until(what: string, check: () => boolean, timeoutMs = 15_000): Promise<void> {
  const start = performance.now();
  while (!check()) {
    if (performance.now() - start > timeoutMs) throw new Error(`timed out waiting for ${what}`);
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });
  }
}

/** Waits until no dialog is open. */
export function dialogClosed(): Promise<void> {
  return until("the dialog to close", () => document.querySelector('[role="dialog"], [role="alertdialog"]') === null);
}
