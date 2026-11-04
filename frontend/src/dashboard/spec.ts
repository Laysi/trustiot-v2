// The component spec: one item of a dashboard version's component list, as the platform's
// components read it (docs/architecture/ai-assistant.md, 「元件模式」). It is data, never code:
// names of datasets, columns, computations and constants, plus a title and units as text. No
// field picks a color, a style, HTML or a URL.
//
// The schema below is bound to the type generated from api/openapi.json (`DashboardComponent`,
// crates/contract/src/api/dashboard/component.rs) with `lib/type-binding.ts`, as `api/devices.ts`
// does: a change in the contract fails `typecheck` here until the schema follows. So a version's
// `spec.components` (`DashboardView.version.spec`) is a `ComponentSpec[]` as the API sends it, and
// the dashboard page hands each item to `DashboardComponent` without an adapter. The one item
// that is not a component is `custom_page`: the page gives it to the sandbox (`isPlatformComponent`).
// The limits the schema cannot know, because they are in the spec's datasets (a cell's row is
// below its dataset's `max_rows`) or in a result (series in all), are checked by `model.ts`, the
// one module that maps a spec to what a component draws.
import type { components } from "../api/schema.gen.ts";
import type { Assert, Equal } from "../lib/type-binding.ts";
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
 * The colors a chart has for its series, in order (`--chart-1` to `--chart-6`, src/styles/global.css),
 * and the contract's `MAX_SERIES`: a chart's `y` columns, and with a `series` column its `y`
 * columns times the column's distinct values. More series than this is an error, never a reused
 * or generated color: the query groups them.
 */
export const MAX_SERIES = 6;
/** `MAX_THRESHOLDS` in component.rs. */
const MAX_THRESHOLDS = 4;
/** `MAX_TABLE_COLUMNS` in component.rs. */
const MAX_TABLE_COLUMNS = 16;

/**
 * Optional fields are `.nullable().optional()`: the API's types allow `null` (the generated types
 * say `string | null`), though Rust leaves out a `None` when it writes.
 */
const OptionalUnit = Unit.nullable().optional();

/**
 * A single value: one cell of a dataset, or a named computation's result. A cell's `row` is
 * below its dataset's `max_rows` (the contract's `row_out_of_range`); the schema cannot know the
 * dataset, so `model.ts` checks it.
 */
const ValueRef = z.discriminatedUnion("kind", [
  z.strictObject({ kind: z.literal("cell"), dataset: Name, column: ColumnName, row: z.number().int().min(0) }),
  z.strictObject({ kind: z.literal("computation"), name: Name }),
]);

/** The contract's `SeriesChart`, which a time series and a bar chart share. */
const seriesChart = <K extends "time_series" | "bar">(kind: K) =>
  z.strictObject({
    kind: z.literal(kind),
    title: Text,
    dataset: Name,
    /** A time column; for a bar chart a text or time column: one group of bars per x. */
    x: ColumnName,
    /** One or more number columns, each a line (or a bar in each group). */
    y: z.array(ColumnName).min(1).max(MAX_SERIES),
    /** A text column whose values split the rows into lines or side-by-side bars, for each `y`. */
    series: ColumnName.nullable().optional(),
    unit: OptionalUnit,
    /** Number constants drawn as reference lines, from low to high. */
    thresholds: z.array(Name).max(MAX_THRESHOLDS).optional(),
  });
const TimeSeriesSpec = seriesChart("time_series");
const BarSpec = seriesChart("bar");

const KpiSpec = z.strictObject({
  kind: z.literal("kpi"),
  title: Text,
  value: ValueRef,
  /** A small chart of how the value moved: a time column and a number column of one dataset. */
  trend: z.strictObject({ dataset: Name, x: ColumnName, y: ColumnName }).nullable().optional(),
  /** A computation shown next to the value (for example last week's figure). */
  compare: Name.nullable().optional(),
  unit: OptionalUnit,
});

const TableSpec = z.strictObject({
  kind: z.literal("table"),
  title: Text,
  dataset: Name,
  columns: z
    .array(z.strictObject({ column: ColumnName, unit: OptionalUnit }))
    .min(1)
    .max(MAX_TABLE_COLUMNS),
});

const StatusSpec = z.strictObject({
  kind: z.literal("status"),
  title: Text,
  value: ValueRef,
  /** Number constants, lowest first: n thresholds make n + 1 bands. */
  thresholds: z.array(Name).min(1).max(MAX_THRESHOLDS),
  unit: OptionalUnit,
});

/** The custom page's place in the list: the page itself is the version's `page`. */
const CustomPageSpec = z.strictObject({ kind: z.literal("custom_page") });

export const ComponentSpecSchema = z.discriminatedUnion("kind", [
  TimeSeriesSpec,
  BarSpec,
  KpiSpec,
  TableSpec,
  StatusSpec,
  CustomPageSpec,
]);

/** One item of a version's component list, as the API sends it. */
export type ComponentSpec = z.infer<typeof ComponentSpecSchema>;
export type TimeSeriesComponent = z.infer<typeof TimeSeriesSpec>;
export type BarComponent = z.infer<typeof BarSpec>;
export type KpiComponent = z.infer<typeof KpiSpec>;
export type TableComponent = z.infer<typeof TableSpec>;
export type StatusComponent = z.infer<typeof StatusSpec>;
export type ValueReference = z.infer<typeof ValueRef>;

/** An item that a component draws: every kind but `custom_page`. */
export type PlatformComponent = Exclude<ComponentSpec, { kind: "custom_page" }>;
export type ComponentKind = PlatformComponent["kind"];

/**
 * Whether a component draws the item. `custom_page` is the one that is not: the page hands it,
 * with the version's `page`, to the sandbox (docs/architecture/ai-assistant.md, 「自訂頁面」), so
 * `DashboardComponent` takes only the others.
 */
export function isPlatformComponent(spec: ComponentSpec): spec is PlatformComponent {
  return spec.kind !== "custom_page";
}

/** The schema and the generated type are one type, in both directions (`lib/type-binding.ts`). */
export type ComponentSpecMatchesContract = Assert<
  Equal<z.output<typeof ComponentSpecSchema>, components["schemas"]["DashboardComponent"]>
>;

/** A component spec from the API, or `null` when it is not one: unknown kinds and fields are refused. */
export function parseComponentSpec(input: unknown): ComponentSpec | null {
  const parsed = ComponentSpecSchema.safeParse(input);
  return parsed.success ? parsed.data : null;
}
