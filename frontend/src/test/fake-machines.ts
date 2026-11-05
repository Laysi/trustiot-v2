import { createElement, type ReactNode } from "react";

import { DeviceApiProvider } from "../api/DeviceApiProvider.tsx";
import { ApiError, type ErrorBody } from "../api/errors.ts";
import { MachineApiProvider } from "../api/pending/MachineApiProvider.tsx";
import {
  type BackfillInput,
  type ChangeEntry,
  type ChangeoverInput,
  type ChangeoverResult,
  type CorrectionInput,
  type FieldChange,
  type Machine,
  type MachineApi,
  type MachineDetail,
  type MachineInput,
  type MachineRelation,
  type MachineSummary,
  type PointAssignment,
  type ProductionRun,
  RUN_OVERLAP_KEY,
} from "../api/pending/machines.ts";
import { FakeDeviceServer, fakeUuid } from "./fake-devices.ts";

/**
 * The field-context service's machine endpoints (`MachineApi`), held in memory, until the real
 * service and its generated client exist (Laysi/trustiot-v2#177). It keeps the rules the design
 * and docs/architecture/field-context.md state: only one record in progress per machine, no two
 * records of a machine overlap, a data point belongs to at most one machine, every change leaves
 * a line in the change record, a changeover is stamped with the server's clock.
 *
 * Failure switches for the states the design draws: `unavailable` (the service does not answer),
 * `plan = "starter"` (403 `entitlement.feature_not_in_plan`), `failNext` (one refused call) and
 * `holdNext` (a call that waits until the test lets it go).
 */

type Method = keyof MachineApi;

export function machineError(
  status: number,
  code: ErrorBody["code"],
  messageKey: string,
  details?: Record<string, string | number>,
): ApiError {
  return new ApiError(status, { code, message_key: messageKey, ...(details ? { details } : {}) });
}

const DAY = 86_400_000;

function sameEnd(end: string | null): number {
  return end === null ? Number.POSITIVE_INFINITY : Date.parse(end);
}

export class FakeMachineServer implements MachineApi {
  readonly machines = new Map<string, Machine>();
  readonly runs = new Map<string, ProductionRun>();
  readonly assignments: PointAssignment[] = [];
  readonly relations: (MachineRelation & { owner: string })[] = [];
  readonly changes = new Map<string, ChangeEntry[]>();
  /** Every call, in order: the method and the machine it named. */
  readonly calls: { method: Method; machineId?: string }[] = [];
  /** The server's clock, and the name the change record gives the user who calls. */
  now: () => number = () => Date.now();
  actor = "Alice Chen";
  plan: "business" | "starter" = "business";
  unavailable = false;
  private readonly failures = new Map<Method, ApiError[]>();
  private readonly holds = new Map<Method, Promise<void>[]>();

  /** The next call of `method` is refused with `error`. */
  failNext(method: Method, error: ApiError): void {
    this.failures.set(method, [...(this.failures.get(method) ?? []), error]);
  }

  /** The next call of `method` waits for the returned function. */
  holdNext(method: Method): () => void {
    let release = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    this.holds.set(method, [...(this.holds.get(method) ?? []), gate]);
    return release;
  }

  private async enter(method: Method, machineId?: string): Promise<void> {
    this.calls.push({ method, ...(machineId === undefined ? {} : { machineId }) });
    const gate = this.holds.get(method)?.shift();
    if (gate) await gate;
    if (this.unavailable) throw machineError(503, "unavailable", "error.unavailable");
    if (this.plan === "starter") {
      throw machineError(403, "quota_exceeded", "entitlement.feature_not_in_plan", { entitlement: "field_context" });
    }
    const failure = this.failures.get(method)?.shift();
    if (failure) throw failure;
  }

  private machine(machineId: string): Machine {
    const machine = this.machines.get(machineId);
    if (machine === undefined) throw machineError(404, "not_found", "error.not_found");
    return machine;
  }

  // Seeding for tests and the preview.

  addMachine(name: string, options: { description?: string; utility?: boolean } = {}): Machine {
    const machine: Machine = {
      machine_id: fakeUuid(),
      name,
      description: options.description ?? null,
      utility: options.utility ?? false,
    };
    this.machines.set(machine.machine_id, machine);
    return machine;
  }

  /** A finished or running record, without a change line (a record that "was always there"). */
  addRun(
    machineId: string,
    partNumber: string,
    startedAt: number | string,
    endedAt: number | string | null,
    options: { workOrder?: string; targetQuantity?: number; source?: ProductionRun["source"]; revisions?: number } = {},
  ): ProductionRun {
    const iso = (value: number | string) => new Date(value).toISOString();
    const run: ProductionRun = {
      run_id: fakeUuid(),
      machine_id: machineId,
      part_number: partNumber,
      started_at: iso(startedAt),
      ended_at: endedAt === null ? null : iso(endedAt),
      work_order: options.workOrder ?? null,
      target_quantity: options.targetQuantity ?? null,
      source: options.source ?? "changeover",
      revisions: options.revisions ?? 0,
    };
    this.runs.set(run.run_id, run);
    return run;
  }

  addChange(machineId: string, entry: Omit<ChangeEntry, "change_id">): void {
    this.changes.set(machineId, [...(this.changes.get(machineId) ?? []), { change_id: fakeUuid(), ...entry }]);
  }

  assign(machineId: string, deviceId: string, pointKey: string): void {
    this.assignments.push({ device_id: deviceId, point_key: pointKey, machine_id: machineId });
  }

  relate(kind: MachineRelation["kind"], fromId: string, toId: string): void {
    this.relations.push({ relation_id: fakeUuid(), kind, from_machine_id: fromId, to_machine_id: toId, owner: fromId });
  }

  // Reading.

  private runsOf(machineId: string): ProductionRun[] {
    return [...this.runs.values()]
      .filter((run) => run.machine_id === machineId)
      .sort((a, b) => Date.parse(b.started_at) - Date.parse(a.started_at));
  }

  private current(machineId: string): ProductionRun | null {
    return this.runsOf(machineId).find((run) => run.ended_at === null) ?? null;
  }

  private detail(machine: Machine): MachineDetail {
    return {
      ...machine,
      current_run: this.current(machine.machine_id),
      assignments: this.assignments.filter((item) => item.machine_id === machine.machine_id),
      relations: this.relations
        .filter((item) => item.from_machine_id === machine.machine_id || item.to_machine_id === machine.machine_id)
        .map(({ owner: _owner, ...relation }) => relation),
    };
  }

  async listMachines(): Promise<MachineSummary[]> {
    await this.enter("listMachines");
    return [...this.machines.values()]
      .sort((a, b) => a.name.localeCompare(b.name))
      .map((machine) => {
        const runs = this.runsOf(machine.machine_id);
        const current = runs.find((run) => run.ended_at === null) ?? null;
        const last =
          current === null
            ? ([...runs].sort((a, b) => Date.parse(b.ended_at ?? "0") - Date.parse(a.ended_at ?? "0"))[0] ?? null)
            : null;
        return {
          ...machine,
          current_run: current,
          last_run: last,
          device_ids: [
            ...new Set(
              this.assignments.filter((item) => item.machine_id === machine.machine_id).map((item) => item.device_id),
            ),
          ],
        };
      });
  }

  async getMachine(machineId: string): Promise<MachineDetail> {
    await this.enter("getMachine", machineId);
    return this.detail(this.machine(machineId));
  }

  async listRuns(machineId: string, range: { from: string; to: string }): Promise<ProductionRun[]> {
    await this.enter("listRuns", machineId);
    this.machine(machineId);
    const from = Date.parse(range.from);
    const to = Date.parse(range.to);
    return this.runsOf(machineId).filter((run) => Date.parse(run.started_at) < to && sameEnd(run.ended_at) > from);
  }

  async listChanges(machineId: string): Promise<ChangeEntry[]> {
    await this.enter("listChanges", machineId);
    this.machine(machineId);
    return [...(this.changes.get(machineId) ?? [])].sort((a, b) => Date.parse(b.at) - Date.parse(a.at));
  }

  async listAssignments(): Promise<PointAssignment[]> {
    await this.enter("listAssignments");
    return this.assignments.map((item) => ({ ...item }));
  }

  // Writing.

  private validName(input: MachineInput): void {
    if (input.name.trim() === "") {
      throw new ApiError(422, {
        code: "validation_failed",
        message_key: "error.validation_failed",
        field_errors: [{ field: "/name", message_key: "validation.required" }],
      });
    }
  }

  async createMachine(input: MachineInput): Promise<Machine> {
    await this.enter("createMachine");
    this.validName(input);
    return this.addMachine(input.name.trim(), {
      ...(input.description === null ? {} : { description: input.description }),
      utility: input.utility,
    });
  }

  async updateMachine(machineId: string, input: MachineInput): Promise<Machine> {
    await this.enter("updateMachine", machineId);
    this.machine(machineId);
    this.validName(input);
    const machine: Machine = {
      machine_id: machineId,
      name: input.name.trim(),
      description: input.description,
      utility: input.utility,
    };
    this.machines.set(machineId, machine);
    return machine;
  }

  async deleteMachine(machineId: string): Promise<void> {
    await this.enter("deleteMachine", machineId);
    this.machine(machineId);
    this.machines.delete(machineId);
    for (const run of [...this.runs.values()]) if (run.machine_id === machineId) this.runs.delete(run.run_id);
    this.changes.delete(machineId);
    for (let index = this.assignments.length - 1; index >= 0; index -= 1) {
      if (this.assignments[index]?.machine_id === machineId) this.assignments.splice(index, 1);
    }
    for (let index = this.relations.length - 1; index >= 0; index -= 1) {
      const relation = this.relations[index];
      if (relation?.from_machine_id === machineId || relation?.to_machine_id === machineId) {
        this.relations.splice(index, 1);
      }
    }
  }

  private log(machineId: string, entry: Omit<ChangeEntry, "change_id" | "actor" | "at">): void {
    this.addChange(machineId, { ...entry, actor: this.actor, at: new Date(this.now()).toISOString() });
  }

  async changeover(machineId: string, input: ChangeoverInput): Promise<ChangeoverResult> {
    await this.enter("changeover", machineId);
    this.machine(machineId);
    const part = input.part_number.trim();
    if (part === "") {
      throw new ApiError(422, {
        code: "validation_failed",
        message_key: "error.validation_failed",
        field_errors: [{ field: "/part_number", message_key: "validation.required" }],
      });
    }
    const at = this.now();
    const running = this.current(machineId);
    let previous: ProductionRun | null = null;
    if (running !== null) {
      previous = { ...running, ended_at: new Date(at).toISOString() };
      this.runs.set(previous.run_id, previous);
    }
    const current = this.addRun(machineId, part, at, null, {
      ...(input.work_order === null ? {} : { workOrder: input.work_order }),
      ...(input.target_quantity === null ? {} : { targetQuantity: input.target_quantity }),
    });
    this.log(machineId, {
      kind: previous === null ? "start" : "changeover",
      run_id: current.run_id,
      part_number: part,
      previous_part_number: previous?.part_number ?? null,
      started_at: current.started_at,
      ended_at: null,
      changes: [],
      reason: null,
    });
    return { previous, current };
  }

  async endRun(machineId: string): Promise<ProductionRun> {
    await this.enter("endRun", machineId);
    this.machine(machineId);
    const running = this.current(machineId);
    if (running === null) throw machineError(409, "conflict", "field_context.no_run_in_progress");
    const ended = { ...running, ended_at: new Date(this.now()).toISOString() };
    this.runs.set(ended.run_id, ended);
    this.log(machineId, {
      kind: "end",
      run_id: ended.run_id,
      part_number: ended.part_number,
      previous_part_number: null,
      started_at: ended.started_at,
      ended_at: ended.ended_at,
      changes: [],
      reason: null,
    });
    return ended;
  }

  /** The other record of `machineId` that overlaps `[start, end)`, if any. */
  private overlapping(machineId: string, start: number, end: number, except?: string): ProductionRun | undefined {
    return this.runsOf(machineId).find(
      (run) => run.run_id !== except && Date.parse(run.started_at) < end && sameEnd(run.ended_at) > start,
    );
  }

  private refuseOverlap(run: ProductionRun): never {
    throw machineError(409, "conflict", RUN_OVERLAP_KEY, {
      run_id: run.run_id,
      part_number: run.part_number,
      started_at: run.started_at,
      ...(run.ended_at === null ? {} : { ended_at: run.ended_at }),
    });
  }

  private invalidRun(field: string): never {
    throw new ApiError(422, {
      code: "validation_failed",
      message_key: "error.validation_failed",
      field_errors: [{ field, message_key: "validation.invalid" }],
    });
  }

  async backfillRun(machineId: string, input: BackfillInput): Promise<ProductionRun> {
    await this.enter("backfillRun", machineId);
    this.machine(machineId);
    const part = input.part_number.trim();
    const start = Date.parse(input.started_at);
    const end = Date.parse(input.ended_at);
    if (part === "") this.invalidRun("/part_number");
    if (Number.isNaN(start) || Number.isNaN(end) || end <= start) this.invalidRun("/ended_at");
    const clash = this.overlapping(machineId, start, end);
    if (clash) this.refuseOverlap(clash);
    const run = this.addRun(machineId, part, start, end, {
      source: "backfill",
      ...(input.work_order === null ? {} : { workOrder: input.work_order }),
      ...(input.target_quantity === null ? {} : { targetQuantity: input.target_quantity }),
    });
    this.log(machineId, {
      kind: "backfill",
      run_id: run.run_id,
      part_number: part,
      previous_part_number: null,
      started_at: run.started_at,
      ended_at: run.ended_at,
      changes: [],
      reason: input.reason,
    });
    return run;
  }

  async correctRun(machineId: string, runId: string, input: CorrectionInput): Promise<ProductionRun> {
    await this.enter("correctRun", machineId);
    this.machine(machineId);
    const run = this.runs.get(runId);
    if (run === undefined || run.machine_id !== machineId) throw machineError(404, "not_found", "error.not_found");
    const part = input.part_number.trim();
    const start = Date.parse(input.started_at);
    const end = input.ended_at === null ? Number.POSITIVE_INFINITY : Date.parse(input.ended_at);
    if (part === "") this.invalidRun("/part_number");
    if (input.reason.trim() === "") this.invalidRun("/reason");
    if (Number.isNaN(start) || Number.isNaN(end) || end <= start) this.invalidRun("/ended_at");
    const clash = this.overlapping(machineId, start, end, runId);
    if (clash) this.refuseOverlap(clash);
    const corrected: ProductionRun = {
      ...run,
      part_number: part,
      started_at: new Date(start).toISOString(),
      ended_at: input.ended_at === null ? null : new Date(end).toISOString(),
      work_order: input.work_order,
      target_quantity: input.target_quantity,
      revisions: run.revisions + 1,
    };
    const differences: FieldChange[] = [];
    const field = (name: FieldChange["field"], before: string | number | null, after: string | number | null) => {
      if (before !== after) {
        differences.push({
          field: name,
          before: before === null ? null : String(before),
          after: after === null ? null : String(after),
        });
      }
    };
    field("part_number", run.part_number, corrected.part_number);
    field("started_at", run.started_at, corrected.started_at);
    field("ended_at", run.ended_at, corrected.ended_at);
    field("work_order", run.work_order, corrected.work_order);
    field("target_quantity", run.target_quantity, corrected.target_quantity);
    this.runs.set(runId, corrected);
    this.log(machineId, {
      kind: "correction",
      run_id: runId,
      part_number: corrected.part_number,
      previous_part_number: null,
      started_at: corrected.started_at,
      ended_at: corrected.ended_at,
      changes: differences,
      reason: input.reason.trim(),
    });
    return corrected;
  }

  async setAssignments(machineId: string, points: readonly { device_id: string; point_key: string }[]): Promise<void> {
    await this.enter("setAssignments", machineId);
    this.machine(machineId);
    for (const point of points) {
      const owner = this.assignments.find(
        (item) => item.device_id === point.device_id && item.point_key === point.point_key,
      );
      if (owner !== undefined && owner.machine_id !== machineId) {
        throw machineError(409, "conflict", "field_context.point_assigned");
      }
    }
    for (let index = this.assignments.length - 1; index >= 0; index -= 1) {
      if (this.assignments[index]?.machine_id === machineId) this.assignments.splice(index, 1);
    }
    for (const point of points) this.assign(machineId, point.device_id, point.point_key);
  }

  async setRelations(
    machineId: string,
    relations: readonly Omit<MachineRelation, "relation_id">[],
  ): Promise<MachineRelation[]> {
    await this.enter("setRelations", machineId);
    this.machine(machineId);
    for (let index = this.relations.length - 1; index >= 0; index -= 1) {
      const relation = this.relations[index];
      if (relation?.from_machine_id === machineId || relation?.to_machine_id === machineId) {
        this.relations.splice(index, 1);
      }
    }
    for (const relation of relations) this.relate(relation.kind, relation.from_machine_id, relation.to_machine_id);
    return this.detail(this.machine(machineId)).relations;
  }
}

/**
 * The UI preview's demo: four machines the design draws and three around them, the devices they
 * use (a `FakeDeviceServer` the machine pages read instead of the app's, so the names match),
 * and a week of records, relative to now.
 */
export function demoMachines(): { machines: FakeMachineServer; devices: FakeDeviceServer } {
  const machines = new FakeMachineServer();
  const devices = new FakeDeviceServer();
  const clock = Date.now();
  const hours = (count: number) => clock - count * 3_600_000;
  const meterPoints = ["電壓", "電流", "有效功率", "累計電量"];
  const pm01 = devices.addDevice({
    name: "電表 PM-01",
    modelName: "電表",
    hardwareId: "demo-gateway-1",
    points: meterPoints.map((key) => ({ key, data_type: "f32" as const })),
  });
  const meter = (name: string) =>
    devices.addDevice({ name, modelId: pm01.model_id, hardwareId: "demo-gateway-1" });
  const pm02 = meter("電表 PM-02");
  const pm03 = meter("電表 PM-03");
  const pm05 = meter("電表 PM-05");
  const vibration = devices.addDevice({
    name: "振動感測 VB-03",
    modelName: "振動感測",
    hardwareId: "demo-gateway-1",
    points: ["RMS 速度", "峰值加速度", "軸承溫度", "狀態碼"].map((key) => ({ key, data_type: "f32" as const })),
  });

  const pressA = machines.addMachine("沖床 A", { description: "200 噸沖床 · 二廠 2 號線" });
  const pressB = machines.addMachine("沖床 B", { description: "160 噸沖床 · 二廠 2 號線" });
  const molder = machines.addMachine("射出機 3", { description: "一廠射出區" });
  const compressor = machines.addMachine("空壓機 A1", {
    description: "供電給沖床 A、沖床 B 的氣動系統。公用設施通常不記生產紀錄。",
    utility: true,
  });
  const panel = machines.addMachine("配電盤 P1", { utility: true });
  const feeder = machines.addMachine("送料機 F2");
  const inspection = machines.addMachine("檢測站 2");

  for (const key of meterPoints) {
    machines.assign(pressA.machine_id, pm01.device_id, key);
    machines.assign(pressB.machine_id, pm02.device_id, key);
    machines.assign(compressor.machine_id, pm03.device_id, key);
    machines.assign(molder.machine_id, pm05.device_id, key);
  }
  machines.assign(pressA.machine_id, vibration.device_id, "RMS 速度");
  machines.assign(pressA.machine_id, vibration.device_id, "峰值加速度");
  machines.assign(feeder.machine_id, vibration.device_id, "軸承溫度");

  machines.relate("power", panel.machine_id, pressA.machine_id);
  machines.relate("drive", compressor.machine_id, pressA.machine_id);
  machines.relate("drive", compressor.machine_id, pressB.machine_id);
  machines.relate("flow", feeder.machine_id, pressA.machine_id);
  machines.relate("flow", pressA.machine_id, inspection.machine_id);

  const a = pressA.machine_id;
  const runA1 = machines.addRun(a, "BRK-1023", hours(6.3), null, { workOrder: "WO-5531", targetQuantity: 1200 });
  machines.addRun(a, "BRK-0988", hours(22), hours(6.3), { workOrder: "WO-5520", revisions: 1 });
  machines.addRun(a, "BRK-1100", hours(24 * 3 + 6), hours(24 * 3 - 4), { source: "backfill" });
  machines.addRun(a, "BRK-1023", hours(24 * 4 + 14), hours(24 * 4 + 1), { workOrder: "WO-5498" });
  machines.addRun(a, "BRK-0988", hours(24 * 5 + 20), hours(24 * 5 + 2));
  machines.addChange(a, {
    kind: "changeover",
    run_id: runA1.run_id,
    actor: "王小明",
    at: new Date(hours(6.3)).toISOString(),
    part_number: "BRK-1023",
    previous_part_number: "BRK-0988",
    started_at: runA1.started_at,
    ended_at: null,
    changes: [],
    reason: null,
  });
  machines.addChange(a, {
    kind: "correction",
    run_id: runA1.run_id,
    actor: "陳小玲",
    at: new Date(hours(20)).toISOString(),
    part_number: "BRK-0988",
    previous_part_number: null,
    started_at: null,
    ended_at: null,
    changes: [{ field: "started_at", before: new Date(hours(21.6)).toISOString(), after: new Date(hours(22)).toISOString() }],
    reason: "交班時忘了按",
  });
  machines.addChange(a, {
    kind: "backfill",
    run_id: runA1.run_id,
    actor: "陳小玲",
    at: new Date(hours(24 * 3 - 5)).toISOString(),
    part_number: "BRK-1100",
    previous_part_number: null,
    started_at: new Date(hours(24 * 3 + 6)).toISOString(),
    ended_at: new Date(hours(24 * 3 - 4)).toISOString(),
    changes: [],
    reason: null,
  });

  machines.addRun(pressB.machine_id, "BRK-0988", hours(16), null);
  machines.addRun(pressB.machine_id, "BRK-1023", hours(40), hours(16));
  machines.addRun(molder.machine_id, "CAP-220", hours(48), hours(30));
  return { machines, devices };
}

/** The UI preview loads this file with the other `fake-*.ts` and reads `preview` (frontend/README.md). */
export const preview = {
  name: "Machines",
  provider: (seed: "normal" | "empty") => {
    const { machines, devices } = seed === "normal" ? demoMachines() : { machines: new FakeMachineServer(), devices: new FakeDeviceServer() };
    const deviceApi = devices.api();
    return function MachinesPreview({ children }: { children: ReactNode }) {
      return createElement(
        MachineApiProvider,
        { api: machines },
        createElement(DeviceApiProvider, { api: deviceApi, children }),
      );
    };
  },
};
