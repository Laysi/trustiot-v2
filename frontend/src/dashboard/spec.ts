// The component spec: one item of a dashboard version's component list, as the platform's
// components read it (docs/architecture/ai-assistant.md, 「元件模式」). It is data, never code:
// names of datasets, columns, computations and constants, plus a title and units as text. No
// field picks a color, a style, HTML or a URL.
//
// This module is a stand-in. The contract's `Spec` (crates/contract/src/api/dashboard/spec.rs)
// gains the component list in Laysi/trustiot-v2#933; until then the shapes below mirror the
// design's field table, and everything else in src/dashboard/ reads specs only through
// `ComponentSpec` and `parseComponentSpec`. When #933 lands, bind this schema to the generated
// type (`lib/type-binding.ts`, as `api/devices.ts` does) or replace it, and adjust the field names
// here and in `model.ts`, the one module that maps a spec to what a component draws.
import { z } from "../lib/zod.ts";

/** Names as the contract writes them (`names.rs`): a dataset, computation or constant name. */
const Name = z.string().regex(/^[a-z][a-z0-9_]{0,47}$/);
/** A result column's name (`is_column_name`). */
const ColumnName = z.string().regex(/^[A-Za-z_][A-Za-z0-9_]{0,63}$/);
/**
 * Free text the component shows as text, 1 to `max` characters. Characters are counted as the
 * contract counts them (`text` in spec.rs, Rust's `chars()`), not in UTF-16 units, so a title
 * the contract accepts is never refused here.
 */
const text = (max: number) => z.string().refine((value) => value.length > 0 && [...value].length <= max);
/** `MAX_TITLE_CHARS` in spec.rs. */
const Text = text(120);
/** `MAX_UNIT_CHARS` in spec.rs. */
const Unit = text(24);

/**
 * The colors a chart has for its series, in order (`--chart-1` to `--chart-6`, src/styles/global.css).
 * More series than this is an error, never a reused or generated color: the query groups them.
 */
export const MAX_SERIES = 6;

/** A single value: one cell of a dataset, or a named computation's result. */
const ValueRef = z.union([
  z.strictObject({ dataset: Name, column: ColumnName, row: z.number().int().min(0).max(1999) }),
  z.strictObject({ computation: Name }),
]);

const TimeSeriesSpec = z.strictObject({
  kind: z.literal("time_series"),
  title: Text,
  dataset: Name,
  /** A time column. */
  x: ColumnName,
  /** One or more number columns, each a line. */
  y: z.array(ColumnName).min(1).max(MAX_SERIES),
  /** A text column whose values split the rows into lines; then `y` has one column. */
  series: ColumnName.optional(),
  /** Lines, or lines with the area under them filled. */
  mark: z.enum(["line", "area"]).optional(),
  unit: Unit.optional(),
  /** Number constants drawn as reference lines. */
  thresholds: z.array(Name).max(4).optional(),
});

const BarSpec = z.strictObject({
  kind: z.literal("bar"),
  title: Text,
  dataset: Name,
  /** A text or time column: one group of bars per row (per value, with `series`). */
  x: ColumnName,
  /** A number column. */
  y: ColumnName,
  /** A text column whose values become bars side by side. */
  series: ColumnName.optional(),
  unit: Unit.optional(),
  thresholds: z.array(Name).max(4).optional(),
});

const KpiSpec = z.strictObject({
  kind: z.literal("kpi"),
  title: Text,
  value: ValueRef,
  /** A small chart of how the value moved: a time column and a number column of one dataset. */
  trend: z.strictObject({ dataset: Name, x: ColumnName, y: ColumnName }).optional(),
  /** A computation shown next to the value as its comparison (for example last week's figure). */
  comparison: Name.optional(),
  unit: Unit.optional(),
});

const TableSpec = z.strictObject({
  kind: z.literal("table"),
  title: Text,
  dataset: Name,
  columns: z
    .array(z.strictObject({ column: ColumnName, unit: Unit.optional() }))
    .min(1)
    .max(64),
});

const StatusSpec = z.strictObject({
  kind: z.literal("status"),
  title: Text,
  value: ValueRef,
  /** Number constants, lowest first: n thresholds make n + 1 bands. */
  thresholds: z.array(Name).min(1).max(4),
  unit: Unit.optional(),
});

export const ComponentSpecSchema = z.discriminatedUnion("kind", [
  TimeSeriesSpec,
  BarSpec,
  KpiSpec,
  TableSpec,
  StatusSpec,
]);

export type ComponentSpec = z.infer<typeof ComponentSpecSchema>;
export type ComponentKind = ComponentSpec["kind"];
export type TimeSeriesComponent = z.infer<typeof TimeSeriesSpec>;
export type BarComponent = z.infer<typeof BarSpec>;
export type KpiComponent = z.infer<typeof KpiSpec>;
export type TableComponent = z.infer<typeof TableSpec>;
export type StatusComponent = z.infer<typeof StatusSpec>;
export type ValueReference = z.infer<typeof ValueRef>;

/** A component spec from the API, or `null` when it is not one: unknown kinds and fields are refused. */
export function parseComponentSpec(input: unknown): ComponentSpec | null {
  const parsed = ComponentSpecSchema.safeParse(input);
  return parsed.success ? parsed.data : null;
}
