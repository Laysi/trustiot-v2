// The history chart's ECharts option: the dashboard's time series (dashboard/chart/options.ts),
// so the device page and an answer's chart look and behave the same, plus what a stored history
// needs on top of it: the band between each bucket's minimum and maximum, the hatched spans
// without readings, and a bool point drawn as steps between 0 and 100 %.
//
// The dashboard's x values are wall-clock times as milliseconds of their UTC fields (the axes
// read them with `useUTC`), so each bucket's instant is moved into the user's time zone here
// (`toWall`) and the window a zoom reports is moved back (`fromWall`).
import type { ChartOption } from "../../dashboard/chart/engine.ts";
import { gapMarkArea } from "../../dashboard/chart/gaps.ts";
import type { ChartEnvironment } from "../../dashboard/chart/InteractiveChart.tsx";
import { chartText, timeSeriesOption } from "../../dashboard/chart/options.ts";
import type { DashboardFormat } from "../../dashboard/format.ts";
import type { Point, TimeSeriesView } from "../../dashboard/model.ts";
import type { Datum, Gap } from "./chart.ts";
import { fromWallClock, offsetAt } from "./time.ts";

type Obj = Record<string, unknown>;

/** An instant as the dashboard chart's x value: the user's wall-clock time in UTC fields. */
export function toWall(instant: number, timeZone: string): number {
  return instant + offsetAt(instant, timeZone);
}

/** The instant at which the user's wall clock shows `wall` (the inverse of `toWall`). */
export function fromWall(wall: number, timeZone: string): number {
  return fromWallClock(wall, timeZone);
}

/**
 * The x-axis bounds of a range. Across a daylight-saving fall-back the wall clock repeats an hour,
 * so a short range can map to equal or reversed wall times: the axis then keeps the range's real
 * length from its start, so it always runs forward.
 */
export function wallBounds(from: number, to: number, timeZone: string): { min: number; max: number } {
  const min = toWall(from, timeZone);
  const max = toWall(to, timeZone);
  return { min, max: max > min ? max : min + Math.max(1, to - from) };
}

/** The line's points and how the stops (the buckets) map onto them. */
export interface HistoryModel {
  points: Point[];
  /** For each bucket in `data` order, its index in `points`; -1 for a bucket the chart does not draw. */
  pointOfStop: number[];
  /** The bucket a point belongs to; a break between runs belongs to none. */
  stopOfPoint: Map<number, number>;
}

/**
 * The buckets as points of the dashboard's series, with a `null` point inside each gap so the
 * line breaks there instead of running across a time with no data. A bucket whose wall-clock time
 * does not come after the one before (the hour a daylight-saving change repeats) is not drawn:
 * the time axis cannot go back.
 */
export function historyModel(data: readonly Datum[], gaps: readonly Gap[], timeZone: string): HistoryModel {
  const breaks = gaps
    .filter(
      (gap) => data.length > 0 && gap.from > (data[0] as Datum).time && gap.to <= (data[data.length - 1] as Datum).time,
    )
    .map((gap) => (gap.from + gap.to) / 2)
    .sort((a, b) => a - b);
  const points: Point[] = [];
  const pointOfStop: number[] = [];
  const stopOfPoint = new Map<number, number>();
  let nextBreak = 0;
  let lastX = Number.NEGATIVE_INFINITY;
  data.forEach((datum, stop) => {
    while (nextBreak < breaks.length && (breaks[nextBreak] as number) < datum.time) {
      const x = toWall(breaks[nextBreak] as number, timeZone);
      if (x > lastX && points.length > 0) {
        points.push({ x, y: null, row: -1 });
        lastX = x;
      }
      nextBreak += 1;
    }
    const x = toWall(datum.time, timeZone);
    if (x <= lastX) {
      pointOfStop.push(-1);
      return;
    }
    pointOfStop.push(points.length);
    stopOfPoint.set(points.length, stop);
    points.push({ x, y: datum.value, row: stop });
    lastX = x;
  });
  return { points, pointOfStop, stopOfPoint };
}

/** The view the dashboard's option builder reads: one series, no thresholds. */
function historyView(model: HistoryModel, title: string): TimeSeriesView {
  const xs = model.points.filter((point) => point.y !== null).map((point) => point.x);
  return {
    kind: "time_series",
    title,
    unit: undefined,
    dataset: "",
    xColumn: "",
    dateOnly: false,
    series: [{ key: "value", label: title, slot: 0, column: "value", points: model.points, ownBreaks: true }],
    xs,
    thresholds: [],
  };
}

export interface HistoryOptionInput {
  data: readonly Datum[];
  kind: "number" | "bool";
  model: HistoryModel;
  gaps: readonly Gap[];
  /** The queried range, as instants: the x axis spans it, so a stalled device's gap reaches its end. */
  from: number;
  to: number;
  timeZone: string;
  /** The text of a gap's label ("48 minutes without readings"). */
  gapLabel(gap: Gap): string;
  /** A share (0 to 1) as the bool axis writes it. */
  percent(value: number): string;
}

export function historyOption(
  input: HistoryOptionInput,
  format: DashboardFormat,
  environment: ChartEnvironment,
  title: string,
  description: string,
): ChartOption {
  const { theme } = environment;
  const { model, kind, timeZone } = input;
  const option = timeSeriesOption(historyView(model, title), format, environment, description);
  const [line, ...helpers] = option.series as Obj[];
  const color = theme.series[0] ?? theme.text;
  const bool = kind === "bool";

  const hole = gapMarkArea({
    gaps: input.gaps,
    theme,
    from: input.from,
    to: input.to,
    x: (instant) => toWall(instant, timeZone),
    label: input.gapLabel,
    text: chartText,
  });

  const mainLine: Obj = {
    ...line,
    // Every bucket is drawn (at most 1500): thinning the line would no longer match the band.
    sampling: undefined,
    markArea: hole,
    ...(bool ? { step: "end", showSymbol: false, areaStyle: { color, opacity: 0.16 } } : {}),
  };

  const band: Obj[] = [];
  if (!bool && model.points.some((point) => point.row >= 0 && bandOf(input.data, point) !== null)) {
    const quiet = {
      type: "line",
      stack: "band",
      symbol: "none",
      silent: true,
      legendHoverLink: false,
      tooltip: { show: false },
      emphasis: { disabled: true },
      connectNulls: false,
    };
    const edges = model.points.map((point) => [point.x, point.row >= 0 ? bandOf(input.data, point) : null] as const);
    band.push(
      {
        ...quiet,
        name: "band-base",
        lineStyle: { opacity: 0 },
        data: edges.map(([x, edge]) => [x, edge === null ? null : edge.min]),
      },
      {
        ...quiet,
        name: "band-range",
        lineStyle: { opacity: 0 },
        areaStyle: { color, opacity: 0.16 },
        data: edges.map(([x, edge]) => [x, edge === null ? null : edge.max - edge.min]),
      },
    );
  }

  const xAxis = option.xAxis as Obj;
  const yAxis = option.yAxis as Obj;
  const bounds = wallBounds(input.from, input.to, timeZone);
  return {
    ...option,
    xAxis: { ...xAxis, ...bounds },
    yAxis: bool
      ? {
          ...yAxis,
          min: 0,
          max: 1,
          interval: 0.5,
          scale: false,
          axisLabel: {
            ...(yAxis.axisLabel as Obj),
            formatter: (value: number) => chartText(input.percent(value)),
          },
        }
      : yAxis,
    // The data series stay first: ECharts' series index is the dashboard's color slot.
    series: [mainLine, ...helpers, ...band],
  };
}

/** The extent of a bucket's readings, or null when it has no minimum and maximum. */
function bandOf(data: readonly Datum[], point: Point): { min: number; max: number } | null {
  const datum = data[point.row];
  if (datum === undefined || datum.min === null || datum.max === null) return null;
  return { min: datum.min, max: datum.max };
}
