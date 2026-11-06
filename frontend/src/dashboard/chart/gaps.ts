// Where a time series has no data, and how a chart shows it: the one rule behind the device
// page's history chart and the dashboard's time series and KPI trend. A hole between two points
// longer than three of the series' usual spacing breaks the line (a `null` point inside it) and,
// on a chart, is drawn as a hatched span; a shorter hole is noise in the line, not a gap.
// Results that group by time leave out the buckets without rows, so the hole is the only trace of
// a time with no data (docs/design/device-page/design.md, 「缺區間」).
import type { Point, SeriesView } from "../model.ts";
import type { ChartOption, ChartTheme } from "./engine.ts";

const SECOND = 1000;

/** A span with no data, in the units of the times it was computed from. */
export interface Gap {
  from: number;
  to: number;
}

/** How long a hole may be before it is a gap: three lengths, or three of the median spacing of `times` (in order). */
export function gapLimit(length: number, times: readonly number[]): number {
  const spacing: number[] = [];
  for (let index = 1; index < times.length; index += 1) {
    spacing.push((times[index] as number) - (times[index - 1] as number));
  }
  spacing.sort((a, b) => a - b);
  const median = spacing.length > 0 ? (spacing[Math.floor(spacing.length / 2)] as number) : 0;
  return 3 * Math.max(length, median, SECOND);
}

/**
 * The spans of `[from, to)` that no entry of `times` (in order, each covering `length`) reaches
 * and that are longer than three of the usual spacing (`gapLimit`); one at the start or the end
 * counts too (a source that stopped reporting). Without any time the whole range is one gap.
 */
export function gapsBetween(times: readonly number[], length: number, from: number, to: number): Gap[] {
  if (!(to > from)) return [];
  const limit = gapLimit(length, times);
  const first = times[0];
  const last = times[times.length - 1];
  if (first === undefined || last === undefined) return [{ from, to }];

  const gaps: Gap[] = [];
  if (first - from > limit) gaps.push({ from, to: first });
  for (let index = 1; index < times.length; index += 1) {
    const before = times[index - 1] as number;
    const after = times[index] as number;
    if (after - (before + length) > limit) gaps.push({ from: before + length, to: after });
  }
  if (to - (last + length) > limit) gaps.push({ from: last + length, to });
  return gaps;
}

/** One entry of a line's data: a point of the series, or the break inside a gap (`index` -1). */
export interface LineEntry {
  x: number;
  y: number | null;
  /** The point's index in the series, or -1 for a break. */
  index: number;
}

/** A series' line as ECharts gets it, and where each point sits in it. */
export interface LineLayout {
  /** The points in order with a `null` entry in the middle of each gap. */
  entries: readonly LineEntry[];
  /** The holes between points that break the line. */
  gaps: readonly Gap[];
  /** For each point of the series, its position in `entries`. */
  entryOfPoint: readonly number[];
}

const LAYOUTS = new WeakMap<readonly Point[], LineLayout>();

/**
 * The line of a series' points (ascending in x) with a break inside each gap. ECharts' data
 * index is a position in `entries`, so a caller that maps a hit back to a point goes through
 * `entries`. Only holes between two points count: the axis runs from the first to the last x.
 * A series whose points carry their own breaks (`ownBreaks`) is laid out as it is.
 */
export function lineLayout(series: Pick<SeriesView, "points" | "ownBreaks">): LineLayout {
  const { points } = series;
  const cached = LAYOUTS.get(points);
  if (cached !== undefined) return cached;
  const first = points[0];
  const last = points[points.length - 1];
  const gaps =
    first === undefined || last === undefined || series.ownBreaks === true
      ? []
      : gapsBetween(
          points.map((point) => point.x),
          0,
          first.x,
          last.x,
        );
  const entries: LineEntry[] = [];
  const entryOfPoint: number[] = [];
  let nextGap = 0;
  points.forEach((point, index) => {
    const gap = gaps[nextGap];
    if (gap !== undefined && gap.to === point.x) {
      entries.push({ x: Math.floor((gap.from + gap.to) / 2), y: null, index: -1 });
      nextGap += 1;
    }
    entryOfPoint.push(entries.length);
    entries.push({ x: point.x, y: point.y, index });
  });
  const layout = { entries, gaps, entryOfPoint };
  LAYOUTS.set(points, layout);
  return layout;
}

/** The hatched fill of a gap: diagonal strokes on a canvas pattern, plain where there is no canvas. */
export function hatch(theme: ChartTheme): unknown {
  try {
    const canvas = document.createElement("canvas");
    canvas.width = 8;
    canvas.height = 8;
    const context = canvas.getContext("2d");
    if (context !== null) {
      context.strokeStyle = theme.grid;
      context.lineWidth = 2;
      context.beginPath();
      context.moveTo(-2, 10);
      context.lineTo(10, -2);
      context.moveTo(-2, 2);
      context.lineTo(2, -2);
      context.moveTo(6, 10);
      context.lineTo(10, 6);
      context.stroke();
      return { image: canvas, repeat: "repeat" };
    }
  } catch {
    // Fall through to a plain fill.
  }
  return theme.grid;
}

/**
 * A span at least this share of the whole range gets its name (the label has its own backing, so
 * it may be wider than a narrow span); a span at an end of the range keeps its label inside the chart.
 */
const LABELLED_SHARE = 0.025;

export interface GapAreaInput {
  gaps: readonly Gap[];
  theme: ChartTheme;
  /** The range the share of a gap is taken from, in the units of `gaps`. */
  from: number;
  to: number;
  /** A gap's edge as an x value of the axis. */
  x(value: number): number;
  /** The text of a gap's label, or `null` for a gap that is hatched without one. */
  label(gap: Gap): string | null;
  /** The label's text as ECharts may draw it (`chartText`). */
  text(text: string): string;
}

/** The `markArea` that hatches `gaps` and names the wide ones. */
export function gapMarkArea(input: GapAreaInput): ChartOption {
  const { theme, from, to } = input;
  const span = Math.max(1, to - from);
  return {
    silent: true,
    animation: false,
    itemStyle: { color: hatch(theme), borderColor: theme.axis, borderType: "dashed", borderWidth: 1 },
    label: {
      position: "insideTop",
      color: theme.text,
      fontSize: 12,
      fontWeight: 600,
      backgroundColor: theme.surface,
      borderColor: theme.grid,
      borderWidth: 1,
      borderRadius: 5,
      padding: [3, 7],
    },
    // A gap whose axis end does not come after its start (inside a repeated hour) is not hatched.
    data: input.gaps
      .filter((gap) => input.x(gap.to) > input.x(gap.from))
      .map((gap) => {
        const text = input.label(gap);
        return [
          {
            xAxis: input.x(gap.from),
            label: {
              show: text !== null && (gap.to - gap.from) / span >= LABELLED_SHARE,
              position:
                (to - gap.to) / span < 0.01
                  ? "insideTopRight"
                  : (gap.from - from) / span < 0.01
                    ? "insideTopLeft"
                    : "insideTop",
              formatter: () => input.text(text ?? ""),
            },
          },
          { xAxis: input.x(gap.to) },
        ];
      }),
  };
}
