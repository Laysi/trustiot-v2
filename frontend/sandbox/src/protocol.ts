// The dashboard sandbox's protocol (Laysi/trustiot-v2#150): the data a page receives, the
// postMessage messages between the host adapter and the sandbox runtime, the references a page
// marks its values with, and the check's verdict. The Rust side is crates/contract/src/api/dashboard;
// both read the JSON fixtures in crates/contract/tests/fixtures/dashboard, so the two cannot drift
// (protocol.test.ts). frontend/sandbox/README.md describes the protocol.
//
// Data that comes from the backend (results, the spec) is parsed with plain objects, which drop
// fields this version does not know: a newer backend may add optional ones. Messages that come
// from the sandbox are parsed strictly: the page runs there, so they are untrusted.
import { z } from "../../src/lib/zod.ts";

/** The protocol version every message carries in its `tq` field. */
export const PROTOCOL = 1;

/** Runtime versions this build ships (`RuntimeVersion::SHIPPED`). */
export const RUNTIMES = [1] as const;

/** Most rows of a dataset (`result::MAX_DATASET_ROWS`). */
export const MAX_DATASET_ROWS = 2000;

/** Most problems in a failed verdict (`check::MAX_PROBLEMS`). */
export const MAX_PROBLEMS = 20;

/** Longest script error message in a problem or an `error` message, in characters. */
export const MAX_SCRIPT_MESSAGE_CHARS = 200;

/** Longest literal number in a problem, in characters. */
export const MAX_LITERAL_CHARS = 32;

/** Most elements and marks under the page's root. */
export const MAX_ELEMENTS = 20_000;
export const MAX_MARKS = 10_000;

/** From the data message to a finished render, and the quiet time that ends it. */
export const RENDER_TIME_LIMIT_MS = 5000;
export const QUIET_PERIOD_MS = 200;

/** The tallest height a `resize` message may report, in CSS pixels. */
export const MAX_FRAME_HEIGHT = 20_000;

/** Longest reference (`names::MAX_REF_LEN`). */
export const MAX_REF_LEN = 160;

/** A dataset, computation or constant name (`names::NAME_PATTERN`). */
export const NAME_PATTERN = /^[a-z][a-z0-9_]{0,47}$/;

/** A column name (`names::COLUMN_NAME_PATTERN`). */
export const COLUMN_NAME_PATTERN = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;

const name = z.string().regex(NAME_PATTERN);
const columnName = z.string().regex(COLUMN_NAME_PATTERN);

// ---------------------------------------------------------------------------------------------
// References

/** A meta field of an `m|…` reference. */
export const META_FIELDS = ["queried_at", "row_count", "range_from", "range_to"] as const;
export type MetaField = (typeof META_FIELDS)[number];

export type Reference =
  | { form: "c"; dataset: string; column: string; row: number }
  | { form: "f"; name: string }
  | { form: "k"; name: string }
  | { form: "m"; dataset: string; field: MetaField }
  | { form: "t"; dataset: string; column: string; value: number | string };

const ROW = /^(0|[1-9][0-9]{0,3})$/;
const TICK_TIME = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}$/;

/** Whether `text` is a finite number exactly as `String(number)` writes it. */
function isNumberText(text: string): boolean {
  if (text.length > 32) return false;
  const number = Number(text);
  return Number.isFinite(number) && String(number) === text;
}

/** Whether `text` is a wall-clock time `YYYY-MM-DDThh:mm:ss.sss` that exists. */
function isTickTime(text: string): boolean {
  if (!TICK_TIME.test(text)) return false;
  const date = new Date(`${text}Z`);
  return !Number.isNaN(date.getTime()) && date.toISOString() === `${text}Z`;
}

/**
 * Parses a reference (crates/contract/src/api/dashboard/names.rs): `null` unless `text` is one in
 * its single canonical spelling.
 */
export function parseReference(text: string): Reference | null {
  if (text.length > MAX_REF_LEN) return null;
  const parts = text.split("|");
  const [form, a = "", b = "", c = ""] = parts;
  switch (form) {
    case "c": {
      if (parts.length !== 4 || !NAME_PATTERN.test(a) || !COLUMN_NAME_PATTERN.test(b) || !ROW.test(c)) return null;
      const row = Number(c);
      return row < MAX_DATASET_ROWS ? { form, dataset: a, column: b, row } : null;
    }
    case "f":
    case "k":
      return parts.length === 2 && NAME_PATTERN.test(a) ? { form, name: a } : null;
    case "m": {
      const field = META_FIELDS.find((candidate) => candidate === b);
      return parts.length === 3 && NAME_PATTERN.test(a) && field !== undefined ? { form, dataset: a, field } : null;
    }
    case "t": {
      if (parts.length !== 4 || !NAME_PATTERN.test(a) || !COLUMN_NAME_PATTERN.test(b)) return null;
      if (isNumberText(c)) return { form, dataset: a, column: b, value: Number(c) };
      return isTickTime(c) ? { form, dataset: a, column: b, value: c } : null;
    }
    default:
      return null;
  }
}

/** The canonical text of a reference. */
export function formatReference(reference: Reference): string {
  switch (reference.form) {
    case "c":
      return `c|${reference.dataset}|${reference.column}|${reference.row}`;
    case "f":
    case "k":
      return `${reference.form}|${reference.name}`;
    case "m":
      return `m|${reference.dataset}|${reference.field}`;
    case "t":
      return `t|${reference.dataset}|${reference.column}|${String(reference.value)}`;
  }
}

const reference = z.string().refine((text) => parseReference(text) !== null, "not a reference");

// ---------------------------------------------------------------------------------------------
// The spec's public part and the dataset results

export const SOURCE_KINDS = ["sql", "telemetry_statistics", "telemetry_usage"] as const;
export const COLUMN_TYPES = ["number", "text", "time", "bool"] as const;
export const SEMANTIC_INTERVALS = ["1m", "5m", "15m", "1h", "1d", "1w", "1mo", "total"] as const;

export const ComputationSchema = z.object({
  name,
  title: z.string(),
  formula: z.string(),
  unit: z.string().optional(),
});

export const ConstantSchema = z.object({
  name,
  title: z.string(),
  value: z.union([z.number(), z.string()]),
  unit: z.string().optional(),
  note: z.string().optional(),
});

export const PublicDatasetSchema = z.object({
  name,
  title: z.string(),
  description: z.string(),
  source: z.enum(SOURCE_KINDS),
  max_rows: z.number().int().min(1).max(MAX_DATASET_ROWS),
});

/** What the page and the check see of a spec (`spec::PublicSpec`). */
export const PublicSpecSchema = z.object({
  datasets: z.array(PublicDatasetSchema),
  computations: z.array(ComputationSchema),
  constants: z.array(ConstantSchema),
});
export type PublicSpec = z.infer<typeof PublicSpecSchema>;

const SemanticRequestSchema = z.object({
  series: z.array(z.object({ device_id: z.string(), point: z.string() })),
  tz: z.string(),
  period: z.string().optional(),
  from: z.string().optional(),
  to: z.string().optional(),
  interval: z.enum(SEMANTIC_INTERVALS),
});

/** A whole spec (`spec::Spec`), as the public API returns it. */
export const SpecSchema = z.object({
  datasets: z.array(
    z.object({
      name,
      title: z.string(),
      description: z.string(),
      max_rows: z.number().int().min(1).max(MAX_DATASET_ROWS),
      source: z.discriminatedUnion("kind", [
        z.object({ kind: z.literal("sql"), sql: z.string() }),
        z.object({ kind: z.literal("telemetry_statistics"), request: SemanticRequestSchema }),
        z.object({ kind: z.literal("telemetry_usage"), request: SemanticRequestSchema }),
      ]),
    }),
  ),
  computations: z.array(ComputationSchema),
  constants: z.array(ConstantSchema),
});
export type Spec = z.infer<typeof SpecSchema>;

/** The public part of a spec: the host adapter gives the sandbox this, never the queries. */
export function publicSpec(spec: Spec): PublicSpec {
  return {
    datasets: spec.datasets.map(({ name, title, description, source, max_rows }) => ({
      name,
      title,
      description,
      source: source.kind,
      max_rows,
    })),
    computations: spec.computations,
    constants: spec.constants,
  };
}

const cell = z.union([z.number(), z.string(), z.boolean(), z.null()]);

const SemanticRangeSchema = z.object({
  tz: z.string(),
  interval: z.enum(SEMANTIC_INTERVALS),
  from: z.string(),
  to: z.string(),
  truncated: z.boolean(),
  earliest: z.string(),
  ends_after_now: z.boolean(),
  resolution: z.enum(["1m", "1h"]),
  queried_at: z.string(),
});

/** One run of one dataset (`result::DatasetResult`). */
export const DatasetResultSchema = z.object({
  dataset: name,
  columns: z.array(z.object({ name: columnName, type: z.enum(COLUMN_TYPES) })),
  rows: z.array(z.array(cell)).max(MAX_DATASET_ROWS),
  row_count: z.number().int().min(0).max(MAX_DATASET_ROWS),
  meta: z.object({
    queried_at: z.string(),
    duration_ms: z.number().int().min(0),
    source: z.discriminatedUnion("kind", [
      z.object({
        kind: z.literal("sql"),
        queryable_from: z.object({
          tenant_telemetry: z.string(),
          tenant_telemetry_1m: z.string(),
          tenant_telemetry_1h: z.string(),
        }),
      }),
      z.object({ kind: z.literal("telemetry_statistics"), range: SemanticRangeSchema }),
      z.object({ kind: z.literal("telemetry_usage"), range: SemanticRangeSchema }),
    ]),
  }),
});
export type DatasetResult = z.infer<typeof DatasetResultSchema>;

export const LOCALES = ["zh-TW", "en"] as const;
export type Locale = (typeof LOCALES)[number];

// ---------------------------------------------------------------------------------------------
// The check (crates/contract/src/api/dashboard/check.rs)

/** What the checker hands `tqCompare` in the isolated world, and the runtime at display time. */
export const CompareInputSchema = z.object({
  runtime: z.literal(1),
  spec: PublicSpecSchema,
  datasets: z.array(DatasetResultSchema),
  locale: z.enum(LOCALES),
  tz: z.string(),
});
export type CompareInput = z.infer<typeof CompareInputSchema>;

/** The check's request (`check::CheckRequest`): the compare input and the page. */
export const CheckRequestSchema = CompareInputSchema.extend({ page: z.string() });
export type CheckRequest = z.infer<typeof CheckRequestSchema>;

export const LITERAL_PLACES = [
  "text",
  "title",
  "alt",
  "aria-label",
  "aria-description",
  "aria-valuetext",
  "aria-valuenow",
  "aria-valuemin",
  "aria-valuemax",
  "aria-roledescription",
  "aria-placeholder",
  "placeholder",
  "value",
  "label",
  "css-content",
  "list-marker",
] as const;
export const GEOMETRY_CHANNELS = ["width", "height", "x", "y", "radius", "angle", "axis"] as const;
export const CONTENT_KINDS = [
  "canvas",
  "image",
  "media",
  "frame",
  "object",
  "form_field",
  "shadow_root",
  "generated_content",
] as const;

const tag = z.string().optional();

/** One failed rule (`check::Problem`); `kind` is a closed set. */
export const ProblemSchema = z.discriminatedUnion("kind", [
  z.strictObject({ kind: z.literal("not_rendered") }),
  z.strictObject({ kind: z.literal("navigated") }),
  z.strictObject({ kind: z.literal("script_error"), message: z.string().max(MAX_SCRIPT_MESSAGE_CHARS).optional() }),
  z.strictObject({ kind: z.literal("unmapped"), reference: reference.optional(), tag }),
  z.strictObject({ kind: z.literal("mismatch"), reference, value: cell, tag }),
  z.strictObject({
    kind: z.literal("literal_number"),
    place: z.enum(LITERAL_PLACES),
    literal: z
      .string()
      .max(MAX_LITERAL_CHARS)
      .regex(/^[\p{N}.,]*\p{N}[\p{N}.,]*$/u),
    tag,
  }),
  z.strictObject({ kind: z.literal("mark_geometry"), reference, channel: z.enum(GEOMETRY_CHANNELS) }),
  z.strictObject({ kind: z.literal("timeout"), phase: z.enum(["render", "check"]) }),
  z.strictObject({ kind: z.literal("too_large"), limit: z.enum(["elements", "marks"]), max: z.number().int() }),
  z.strictObject({ kind: z.literal("unsupported_content"), content: z.enum(CONTENT_KINDS) }),
]);
export type Problem = z.infer<typeof ProblemSchema>;

/** The check's answer (`check::CheckVerdict`), and what `tqCompare` returns. */
export const CheckVerdictSchema = z.discriminatedUnion("verdict", [
  z.strictObject({
    verdict: z.literal("passed"),
    report: z.strictObject({
      marks: z.array(z.strictObject({ reference, value: cell })).max(MAX_MARKS),
    }),
  }),
  z.strictObject({ verdict: z.literal("failed"), problems: z.array(ProblemSchema).min(1).max(MAX_PROBLEMS) }),
]);
export type CheckVerdict = z.infer<typeof CheckVerdictSchema>;

// ---------------------------------------------------------------------------------------------
// postMessage: host adapter -> sandbox runtime

/** Starts the runtime: the page and what it may know, once per frame. */
export const InitMessageSchema = z.strictObject({
  tq: z.literal(PROTOCOL),
  type: z.literal("init"),
  runtime: z.literal(1),
  page: z.string(),
  spec: PublicSpecSchema,
  locale: z.enum(LOCALES),
  tz: z.string(),
  theme: z.enum(["light", "dark"]),
  mode: z.enum(["view", "check"]),
});

/** One dataset in a `data` message: its result, or why it has none (a message key). */
export const DeliveredDatasetSchema = z.discriminatedUnion("status", [
  z.strictObject({ status: z.literal("ok"), result: DatasetResultSchema }),
  z.strictObject({ status: z.literal("error"), dataset: name, message_key: z.string() }),
]);

/** Every dataset of the spec, all at once; each new `generation` renders again. */
export const DataMessageSchema = z.strictObject({
  tq: z.literal(PROTOCOL),
  type: z.literal("data"),
  generation: z.number().int().min(1),
  datasets: z.array(DeliveredDatasetSchema),
});

export const HostMessageSchema = z.discriminatedUnion("type", [InitMessageSchema, DataMessageSchema]);
export type HostMessage = z.infer<typeof HostMessageSchema>;

// ---------------------------------------------------------------------------------------------
// postMessage: sandbox runtime -> host adapter (untrusted: the page can send these too)

export const SandboxMessageSchema = z.discriminatedUnion("type", [
  /** The runtime loaded and waits for `init`. */
  z.strictObject({ tq: z.literal(PROTOCOL), type: z.literal("ready"), runtime: z.literal(1) }),
  /** `render` finished for this generation and the document stayed quiet. */
  z.strictObject({ tq: z.literal(PROTOCOL), type: z.literal("rendered"), generation: z.number().int().min(1) }),
  /** A page script threw or `render` failed; the message is the page's and only ever shown as such. */
  z.strictObject({
    tq: z.literal(PROTOCOL),
    type: z.literal("error"),
    phase: z.enum(["page", "render"]),
    message: z.string().max(MAX_SCRIPT_MESSAGE_CHARS).optional(),
  }),
  /** The document's height, for the frame. */
  z.strictObject({
    tq: z.literal(PROTOCOL),
    type: z.literal("resize"),
    height: z.number().int().min(0).max(MAX_FRAME_HEIGHT),
  }),
  /** The user chose a marked value: the host shows where it comes from (display mode only). */
  z.strictObject({ tq: z.literal(PROTOCOL), type: z.literal("source"), reference }),
]);
export type SandboxMessage = z.infer<typeof SandboxMessageSchema>;

/** A message from the sandbox, or `null` for anything that is not exactly one. */
export function parseSandboxMessage(data: unknown): SandboxMessage | null {
  const parsed = SandboxMessageSchema.safeParse(data);
  return parsed.success ? parsed.data : null;
}
