import type { CreateDeviceInput, UpdateDeviceInput } from "../../api/assets.ts";
import type { Device } from "../../api/devices.ts";
import { clientProblem, type Problem } from "../form-problems.ts";

/** The inputs of the device form, as the text the user typed or chose. */
export interface DeviceFormValues {
  name: string;
  hardwareId: string;
  modelId: string;
  transport: Device["transport"];
  host: string;
  port: string;
  unitId: string;
  pollMs: string;
}

/** The fields of the form, as the server names them in a field error's pointer. */
export const DEVICE_FIELDS = [
  "name",
  "hardware_id",
  "model_id",
  "transport",
  "host",
  "port",
  "unit_id",
  "poll_interval_ms",
] as const;
export type DeviceField = (typeof DEVICE_FIELDS)[number];

/** The refusals that are 409s without a field error, and the input each one is about. */
export const DEVICE_CONFLICT_FIELDS: Readonly<Record<string, string>> = {
  "device.name_taken": "/name",
  "device.unit_taken": "/unit_id",
  "device.transport_mismatch": "/transport",
};

/** The polling intervals the form offers, in milliseconds (design 「取樣」): 1 s to 1 h. */
export const POLL_CHOICES: readonly number[] = [1_000, 10_000, 30_000, 60_000, 300_000, 900_000, 3_600_000];

const MAX_NAME = 40;
const MAX_HOST = 253;
const NAME_FORMAT = /^[a-z][a-z0-9]*(-[a-z0-9]+)*$/;

/** The values of a new device: Modbus TCP on its usual port, unit 1, polled as often as the plan and a minute allow. */
export function newDeviceValues(
  options: { gateway?: string; gateways?: readonly string[]; modelId?: string; minPollMs?: number } = {},
): DeviceFormValues {
  const minimum = options.minPollMs ?? 0;
  const poll = POLL_CHOICES.find((choice) => choice >= 60_000 && choice >= minimum) ?? 60_000;
  return {
    name: "",
    hardwareId: options.gateway ?? (options.gateways?.length === 1 ? (options.gateways[0] as string) : ""),
    modelId: options.modelId ?? "",
    transport: "modbus_tcp",
    host: "",
    port: "502",
    unitId: "1",
    pollMs: String(poll),
  };
}

/** The values of an existing device, to edit. */
export function deviceValues(device: Device): DeviceFormValues {
  return {
    name: device.name,
    hardwareId: device.hardware_id,
    modelId: device.model_id,
    transport: device.transport,
    host: device.host,
    port: String(device.port),
    unitId: String(device.unit_id),
    pollMs: String(device.poll_interval_ms),
  };
}

/** The unit ID range of a transport: Modbus TCP 0–255, RTU over TCP 1–247. */
export function unitRange(transport: Device["transport"]): { min: number; max: number } {
  return transport === "rtu_over_tcp" ? { min: 1, max: 247 } : { min: 0, max: 255 };
}

function whole(text: string): number | null {
  return /^\d+$/.test(text.trim()) ? Number(text.trim()) : null;
}

/**
 * What the page can tell is wrong before sending: a missing input, a malformed name, a number out
 * of its range. Everything that needs the tenant's data (a taken name, a taken unit ID, a plan's
 * limit) is the server's to refuse, and shows up the same way (`problemsOf`).
 */
export function validateDevice(values: DeviceFormValues, mode: "create" | "edit"): Problem[] {
  const problems: Problem[] = [];
  if (mode === "create") {
    const name = values.name.trim();
    if (name === "") problems.push(clientProblem("/name", "device.field_required"));
    else if (name.length > MAX_NAME || !NAME_FORMAT.test(name)) {
      problems.push(clientProblem("/name", "device.name_invalid", { max: MAX_NAME }));
    }
    if (values.hardwareId === "") problems.push(clientProblem("/hardware_id", "device_form.choose_gateway"));
    if (values.modelId === "") problems.push(clientProblem("/model_id", "device.model_id_invalid"));
  }
  const host = values.host.trim();
  if (host === "") problems.push(clientProblem("/host", "device.field_required"));
  else if (new TextEncoder().encode(host).length > MAX_HOST) {
    problems.push(clientProblem("/host", "device.host_invalid", { max: MAX_HOST }));
  }
  const port = whole(values.port);
  if (port === null || port < 1 || port > 65_535) {
    problems.push(clientProblem("/port", "device.port_invalid", { min: 1, max: 65_535 }));
  }
  const unit = whole(values.unitId);
  const range = unitRange(values.transport);
  if (unit === null || unit < range.min || unit > range.max) {
    problems.push(clientProblem("/unit_id", "device.unit_id_invalid", range));
  }
  const poll = whole(values.pollMs);
  if (poll === null || poll < 1_000 || poll > 86_400_000) {
    problems.push(clientProblem("/poll_interval_ms", "device.poll_interval_invalid", { min: 1_000, max: 86_400_000 }));
  }
  return problems;
}

/** The body of `POST /api/assets/v1/devices` for values that passed `validateDevice`. */
export function createBody(values: DeviceFormValues): CreateDeviceInput {
  return {
    name: values.name.trim(),
    hardware_id: values.hardwareId,
    model_id: values.modelId,
    transport: values.transport,
    host: values.host.trim(),
    port: Number(values.port),
    unit_id: Number(values.unitId),
    poll_interval_ms: Number(values.pollMs),
  };
}

/**
 * The body of `PUT /api/assets/v1/devices/{id}`. The update replaces every setting: a field
 * left out goes back to its default (unit 1, poll every minute), so the whole form is always sent.
 */
export function updateBody(values: DeviceFormValues, revision: number): UpdateDeviceInput {
  return {
    revision,
    transport: values.transport,
    host: values.host.trim(),
    port: Number(values.port),
    unit_id: Number(values.unitId),
    poll_interval_ms: Number(values.pollMs),
  };
}

/** The labels of the settings a conflicting edit and the latest version can differ in. */
export const COMPARED_SETTINGS = ["transport", "host", "port", "unitId", "pollMs"] as const;
export type ComparedSetting = (typeof COMPARED_SETTINGS)[number];

/** The settings in which `mine` differs from `latest`. */
export function changedSettings(mine: DeviceFormValues, latest: DeviceFormValues): ComparedSetting[] {
  return COMPARED_SETTINGS.filter((setting) => String(mine[setting]).trim() !== String(latest[setting]).trim());
}
