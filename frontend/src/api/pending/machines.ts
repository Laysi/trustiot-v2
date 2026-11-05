// Pending Laysi/trustiot-v2#177: replace with a module over the generated client when api/openapi.json has these endpoints.
import { ApiError } from "../errors.ts";

/**
 * The field-context service's machine and production-record endpoints, as the machines screens
 * (Laysi/trustiot-v2#178) use them. The service does not exist yet, so this is the interface the
 * screens were built against; `src/test/fake-machines.ts` implements it in memory. What the
 * backend issue (Laysi/trustiot-v2#177) and docs/architecture/field-context.md fix: machines are
 * entities apart from devices, a data point can belong to a machine, relations are power, drive
 * and upstream or downstream process, a production record has a part number, a start and an end,
 * an optional work order and target quantity, only one record per machine is in progress, and
 * every change leaves a record. The rest (field names, the shape of the two refusals below, the
 * "utility" flag that the design draws) is an assumption of the screens, listed in the pull request.
 */

/** A machine. A utility (an air compressor) usually keeps no production records. */
export interface Machine {
  machine_id: string;
  name: string;
  description: string | null;
  utility: boolean;
}

/**
 * One production record of a machine. `ended_at` is `null` while it is in progress. A record made
 * by a changeover or by starting production starts at the time the server noted; `backfill` is one
 * entered afterwards. `revisions` counts the corrections.
 */
export interface ProductionRun {
  run_id: string;
  machine_id: string;
  part_number: string;
  /** RFC 3339. */
  started_at: string;
  ended_at: string | null;
  work_order: string | null;
  target_quantity: number | null;
  source: "changeover" | "backfill";
  revisions: number;
}

/** A data point of a device that belongs to a machine (`point_key` is the key in the device's model). */
export interface PointAssignment {
  device_id: string;
  point_key: string;
  machine_id: string;
}

export type RelationKind = "power" | "drive" | "flow";

/**
 * `from` supplies power to `to` (`power`), drives it (`drive`), or is the process before it
 * (`flow`): the minimal set of docs/architecture/field-context.md, point 1.
 */
export interface MachineRelation {
  relation_id: string;
  kind: RelationKind;
  from_machine_id: string;
  to_machine_id: string;
}

/** A machine in the list, with the records the cards show. */
export interface MachineSummary extends Machine {
  current_run: ProductionRun | null;
  /** The newest record that ended, when none is in progress. */
  last_run: ProductionRun | null;
  /** The devices that have at least one data point assigned to this machine. */
  device_ids: string[];
}

export interface MachineDetail extends Machine {
  current_run: ProductionRun | null;
  assignments: PointAssignment[];
  relations: MachineRelation[];
}

export type ChangeKind = "start" | "changeover" | "end" | "backfill" | "correction";

/** What a correction changed in one field; `null` is an empty value. */
export interface FieldChange {
  field: "part_number" | "started_at" | "ended_at" | "work_order" | "target_quantity";
  before: string | null;
  after: string | null;
}

/** One line of the change record (the event layer, checklist K2): who did what, when, and why. */
export interface ChangeEntry {
  change_id: string;
  kind: ChangeKind;
  run_id: string;
  /** Who made the change, as the audit trail names them. */
  actor: string;
  /** RFC 3339. */
  at: string;
  part_number: string;
  /** The part that ended in a changeover. */
  previous_part_number: string | null;
  /** A backfill's record, and a correction's differences. */
  started_at: string | null;
  ended_at: string | null;
  changes: FieldChange[];
  reason: string | null;
}

export interface MachineInput {
  name: string;
  description: string | null;
  utility: boolean;
}

/** A changeover, or the start of production when nothing is in progress: the server notes the time. */
export interface ChangeoverInput {
  part_number: string;
  work_order: string | null;
  target_quantity: number | null;
}

export interface ChangeoverResult {
  /** The record that ended, `null` when production started from nothing. */
  previous: ProductionRun | null;
  current: ProductionRun;
}

/** A record entered afterwards; the reason is optional (design decision Q9). */
export interface BackfillInput {
  part_number: string;
  started_at: string;
  ended_at: string;
  work_order: string | null;
  target_quantity: number | null;
  reason: string | null;
}

/** A correction; the reason is required (design decision Q9). `ended_at` stays `null` for the record in progress. */
export interface CorrectionInput {
  part_number: string;
  started_at: string;
  ended_at: string | null;
  work_order: string | null;
  target_quantity: number | null;
  reason: string;
}

export interface MachineApi {
  listMachines(signal?: AbortSignal): Promise<MachineSummary[]>;
  getMachine(machineId: string, signal?: AbortSignal): Promise<MachineDetail>;
  createMachine(input: MachineInput): Promise<Machine>;
  updateMachine(machineId: string, input: MachineInput): Promise<Machine>;
  deleteMachine(machineId: string): Promise<void>;
  /** Records that overlap `[from, to)`, newest first. */
  listRuns(machineId: string, range: { from: string; to: string }, signal?: AbortSignal): Promise<ProductionRun[]>;
  /** Every change of the machine, newest first. */
  listChanges(machineId: string, signal?: AbortSignal): Promise<ChangeEntry[]>;
  changeover(machineId: string, input: ChangeoverInput): Promise<ChangeoverResult>;
  endRun(machineId: string): Promise<ProductionRun>;
  /** Refused with `RUN_OVERLAP_KEY` when the time overlaps another record. */
  backfillRun(machineId: string, input: BackfillInput): Promise<ProductionRun>;
  /** Refused with `RUN_OVERLAP_KEY` when the new time overlaps another record. */
  correctRun(machineId: string, runId: string, input: CorrectionInput): Promise<ProductionRun>;
  /** Every machine's assigned points: a point belongs to at most one machine. */
  listAssignments(signal?: AbortSignal): Promise<PointAssignment[]>;
  /** Replaces the points of `machineId`; a point of another machine is refused (409). */
  setAssignments(machineId: string, points: readonly { device_id: string; point_key: string }[]): Promise<void>;
  /** Replaces the machine's relations. */
  setRelations(machineId: string, relations: readonly Omit<MachineRelation, "relation_id">[]): Promise<MachineRelation[]>;
}

/** The message key of the 409 a backfill or a correction gets when the time overlaps another record. */
export const RUN_OVERLAP_KEY = "field_context.run_overlap";

/** The record a refused backfill or correction ran into. */
export interface RunOverlap {
  runId: string;
  partNumber: string;
  startedAt: string;
  endedAt: string | null;
}

/** The conflicting record when `error` is the overlap refusal (`details` carries it), else `null`. */
export function runOverlap(error: unknown): RunOverlap | null {
  if (!(error instanceof ApiError) || error.body?.message_key !== RUN_OVERLAP_KEY) return null;
  const details = error.body.details;
  const runId = details?.run_id;
  const partNumber = details?.part_number;
  const startedAt = details?.started_at;
  if (typeof runId !== "string" || typeof partNumber !== "string" || typeof startedAt !== "string") return null;
  const endedAt = details?.ended_at;
  return { runId, partNumber, startedAt, endedAt: typeof endedAt === "string" ? endedAt : null };
}

/** The 403 of a plan without the feature (docs/product/plans.md): Starter cannot use machines. */
export function isNotInPlan(error: unknown): boolean {
  return (
    error instanceof ApiError && error.status === 403 && error.body?.message_key === "entitlement.feature_not_in_plan"
  );
}

/** The field-context service does not answer (its own 503, a timeout, or no response): the design's state 8. */
export function isServiceUnavailable(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    (error.status === 0 || error.body?.code === "unavailable" || error.body?.code === "timeout" || error.status === 503)
  );
}
