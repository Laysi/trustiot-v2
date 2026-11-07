import { MODEL_DOCUMENT_FORMAT, MODEL_DOCUMENT_VERSION } from "../../api/assets.ts";
import type { DataType, DeviceModelPoint } from "../../api/devices.ts";
import { DeviceModelPointSchema } from "../../api/devices.ts";
import { z } from "../../lib/zod.ts";
import { clientProblem, type Problem } from "../form-problems.ts";

export type ByteOrder = "abcd" | "badc" | "cdab" | "dcba";

export const DATA_TYPES: readonly DataType[] = [
  "bool",
  "u16",
  "i16",
  "u32",
  "i32",
  "u64",
  "i64",
  "f32",
  "f64",
  "string",
];
export const BYTE_ORDERS: readonly ByteOrder[] = ["abcd", "badc", "cdab", "dcba"];
/** The Modbus read functions: coils, discrete inputs, holding registers, input registers. */
export const FUNCTION_CODES: readonly number[] = [1, 2, 3, 4];

/** The inputs of one data point, as text; `id` keeps a row's identity while rows move and go. */
export interface PointDraft {
  id: number;
  key: string;
  functionCode: string;
  address: string;
  dataType: DataType;
  byteOrder: ByteOrder;
  length: string;
  scale: string;
  offset: string;
  description: string;
  /**
   * The point's unit as the model has it. The editor has no input for it yet
   * (Laysi/trustiot-v2#126): it is kept as read, so saving the points never drops a unit.
   */
  unit: string | null;
}

/** The fields of a data point, as the server names them in a pointer (`/points/2/address`). */
export const POINT_FIELDS = [
  "key",
  "function_code",
  "address",
  "data_type",
  "byte_order",
  "length",
  "scale",
  "offset",
  "description",
] as const;
export type PointField = (typeof POINT_FIELDS)[number];

/** The refusals that are 409s without a field error, and the input each is about. */
export const MODEL_CONFLICT_FIELDS: Readonly<Record<string, string>> = {
  "device_model.name_taken": "/name",
};

export const MAX_POINTS = 1000;
const MAX_KEY = 64;
const MAX_DESCRIPTION = 512;
const MAX_STRING_REGISTERS = 125;
const KEY_FORMAT = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
export const NAME_FORMAT = /^[a-z][a-z0-9]*(-[a-z0-9]+)*$/;
export const MAX_NAME = 40;

/** A data point nobody has filled in: a holding register, an unsigned 16-bit value. */
export function blankPoint(id: number): PointDraft {
  return {
    id,
    key: "",
    functionCode: "3",
    address: "0",
    dataType: "u16",
    byteOrder: "abcd",
    length: "",
    scale: "",
    offset: "",
    description: "",
    unit: null,
  };
}

export function pointDraft(point: DeviceModelPoint, id: number): PointDraft {
  return {
    id,
    key: point.key,
    functionCode: String(point.function_code),
    address: String(point.address),
    dataType: point.data_type,
    byteOrder: point.byte_order ?? "abcd",
    length: point.length == null ? "" : String(point.length),
    scale: point.scale == null ? "" : String(point.scale),
    offset: point.offset == null ? "" : String(point.offset),
    description: point.description ?? "",
    unit: point.unit ?? null,
  };
}

export function isNumeric(dataType: DataType): boolean {
  return dataType !== "bool" && dataType !== "string";
}

/** Coils and discrete inputs read single bits: only `bool`. */
export function dataTypesFor(functionCode: string): readonly DataType[] {
  return functionCode === "1" || functionCode === "2" ? ["bool"] : DATA_TYPES;
}

/** A bit has no byte order, and a string uses only the two orders that keep the characters in place. */
export function byteOrdersFor(dataType: DataType): readonly ByteOrder[] {
  if (dataType === "bool") return [];
  return dataType === "string" ? ["abcd", "badc"] : BYTE_ORDERS;
}

/**
 * The address a manual prints for a 0-based protocol address (40001 for holding register 0), so a
 * person can compare. Coils are 0xxxx, discrete inputs 1xxxx, input registers 3xxxx, holding
 * registers 4xxxx; past address 9998 the manual's number has six digits.
 */
export function manualAddress(functionCode: string, address: string): string | null {
  const prefix = { "1": "0", "2": "1", "3": "4", "4": "3" }[functionCode];
  if (prefix === undefined || !/^\d+$/.test(address)) return null;
  const number = Number(address);
  if (number > 65_535) return null;
  return `${prefix}${String(number + 1).padStart(number < 9_999 ? 4 : 5, "0")}`;
}

function whole(text: string): number | null {
  return /^\d+$/.test(text.trim()) ? Number(text.trim()) : null;
}

function finite(text: string): number | null {
  if (text.trim() === "") return null;
  const value = Number(text.trim());
  return Number.isFinite(value) ? value : null;
}

/**
 * The data point the draft says, as the API takes it: a field that does not apply to the data
 * type (a scale on a bool, a length on a number, a byte order on a bit) is left out, as the
 * server refuses those combinations. The draft has to have passed `validatePoints`.
 */
export function pointOf(draft: PointDraft): DeviceModelPoint {
  const point: DeviceModelPoint = {
    key: draft.key.trim(),
    function_code: Number(draft.functionCode),
    address: Number(draft.address),
    data_type: draft.dataType,
  };
  if (draft.dataType !== "bool") point.byte_order = draft.byteOrder;
  if (draft.dataType === "string") point.length = Number(draft.length);
  if (isNumeric(draft.dataType)) {
    if (draft.scale.trim() !== "") point.scale = Number(draft.scale);
    if (draft.offset.trim() !== "") point.offset = Number(draft.offset);
  }
  const description = draft.description.trim();
  if (description !== "") point.description = description;
  if (draft.unit !== null) point.unit = draft.unit;
  return point;
}

/** What the page can tell is wrong in one data point before sending; the server checks the rest (overlaps, the end of the address space). */
function pointProblems(draft: PointDraft, index: number, firstOfKey: Map<string, number>): Problem[] {
  const at = (field: PointField) => `/points/${index}/${field}`;
  const problems: Problem[] = [];
  const key = draft.key.trim();
  if (!KEY_FORMAT.test(key)) {
    problems.push(clientProblem(at("key"), "device_model.point_key_invalid", { max: MAX_KEY }));
  } else if (firstOfKey.get(key) !== index) {
    problems.push(clientProblem(at("key"), "device_model.point_key_duplicate"));
  }
  const address = whole(draft.address);
  if (address === null || address > 65_535) {
    problems.push(clientProblem(at("address"), "device_model.address_invalid", { min: 0, max: 65_535 }));
  }
  if (draft.dataType === "string") {
    const length = whole(draft.length);
    if (draft.length.trim() === "") problems.push(clientProblem(at("length"), "device_model.length_required"));
    else if (length === null || length < 1 || length > MAX_STRING_REGISTERS) {
      problems.push(clientProblem(at("length"), "device_model.length_invalid", { min: 1, max: MAX_STRING_REGISTERS }));
    }
  }
  if (isNumeric(draft.dataType)) {
    const scale = draft.scale.trim() === "" ? undefined : finite(draft.scale);
    const offset = draft.offset.trim() === "" ? undefined : finite(draft.offset);
    if (scale === null || scale === 0) problems.push(clientProblem(at("scale"), "device_model.scaling_invalid"));
    if (offset === null) problems.push(clientProblem(at("offset"), "device_model.scaling_invalid"));
  }
  if ([...draft.description.trim()].length > MAX_DESCRIPTION) {
    problems.push(clientProblem(at("description"), "device_model.description_invalid", { max: MAX_DESCRIPTION }));
  }
  return problems;
}

export function validatePoints(drafts: readonly PointDraft[]): Problem[] {
  const firstOfKey = new Map<string, number>();
  drafts.forEach((draft, index) => {
    const key = draft.key.trim();
    if (!firstOfKey.has(key)) firstOfKey.set(key, index);
  });
  const problems = drafts.flatMap((draft, index) => pointProblems(draft, index, firstOfKey));
  if (drafts.length > MAX_POINTS)
    problems.push(clientProblem("/points", "device_model.too_many_points", { max: MAX_POINTS }));
  return problems;
}

/** The name of a new model. */
export function validateName(name: string): Problem[] {
  const text = name.trim();
  if (text === "") return [clientProblem("/name", "device_model.field_required")];
  if (text.length > MAX_NAME || !NAME_FORMAT.test(text)) {
    return [clientProblem("/name", "device_model.name_invalid", { max: MAX_NAME })];
  }
  return [];
}

/** The index a pointer into the points names (`/points/2/address` and `/points/2` are 2), if it names one. */
export function pointIndexOf(pointer: string): number | null {
  const match = /^\/points\/(\d+)(\/|$)/.exec(pointer);
  return match ? Number(match[1]) : null;
}

/** What the import accepts in the browser: the exported document's shape (`DeviceModelDocument`). */
const ImportFileSchema = z.object({
  format: z.literal(MODEL_DOCUMENT_FORMAT),
  format_version: z.literal(MODEL_DOCUMENT_VERSION),
  name: z.string(),
  points: z.array(DeviceModelPointSchema),
});
export type ImportFile = z.output<typeof ImportFileSchema>;

/** The largest file the import reads: the service refuses a document far smaller than this anyway. */
export const MAX_IMPORT_BYTES = 1_048_576;

export type ImportParse =
  | { ok: true; file: ImportFile }
  | { ok: false; reason: "not_json" | "format" | "version" | "shape" };

/**
 * Checks an uploaded file's text in the browser before it is sent: valid JSON, the exported
 * document's `format` and `format_version`, and the shape of the name and data points. The service
 * applies the model rules (names, addresses, overlaps) and its errors are listed as it gives them.
 */
export function parseImportFile(text: string): ImportParse {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return { ok: false, reason: "not_json" };
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) return { ok: false, reason: "format" };
  const record = value as Record<string, unknown>;
  if (record.format !== MODEL_DOCUMENT_FORMAT) return { ok: false, reason: "format" };
  if (record.format_version !== MODEL_DOCUMENT_VERSION) return { ok: false, reason: "version" };
  const parsed = ImportFileSchema.safeParse(value);
  return parsed.success ? { ok: true, file: parsed.data } : { ok: false, reason: "shape" };
}
