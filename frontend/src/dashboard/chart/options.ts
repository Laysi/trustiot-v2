// The ECharts options of the dashboard charts: the default look (frontend/README.md, 「儀表板元件
// 的圖表套件」). Colors and the font come from the theme's custom properties (engine.ts
// `readTheme`), so a spec never picks one. Every label ECharts draws as text goes through the
// component's formatter and `chartText` and is a string, never HTML: axis labels and threshold
// labels (a constant's value).
//
// No value from the data or the spec is a name ECharts keys anything by. ECharts keeps the
// legend's selection and the series' colors in plain objects keyed by series name and calls
// their `hasOwnProperty`, so a series named `hasOwnProperty` (a text cell of the series column)
// made `setOption` throw. Series are named `seriesName(slot)`, categories by their index, and the
// threshold lines carry no name; the names people read stay in the React legend and tooltip. The
// labels ECharts draws are the one way text from the data reaches it, and `chartText` keeps them
// out of the plain object its text measuring keys by line.
import type { DashboardFormat } from "../format.ts";
import type { BarView, SeriesView, Threshold, TimeSeriesView } from "../model.ts";
import type { ChartOption, ChartTheme } from "./engine.ts";
import type { ChartEnvironment } from "./InteractiveChart.tsx";

/**
 * The name of the helper series that carries the threshold lines. Its points are invisible and
 * only stretch the value axis over every threshold: ECharts fits the axis to series data and
 * ignores mark lines, and fixing the axis ends instead would cost its round tick values.
 */
export const THRESHOLD_SERIES = "thresholds";

/** A data series' name in the option: its color slot, never the series' value or column. */
export function seriesName(slot: number): string {
  return `s${slot}`;
}

/** A character that draws nothing and joins the text around it (U+2060 WORD JOINER). */
const WORD_JOINER = "\u2060";

/**
 * `text` as ECharts may draw it. zrender caches each line's width in a plain object keyed by the
 * line, so a line named like an `Object.prototype` member (a category `__proto__`) finds the
 * prototype there, and the cache's linked list then writes `prev` and `next` onto
 * `Object.prototype` for the whole page. Such a line gets a word joiner in front: it draws the
 * same and is a key of its own.
 */
export function chartText(text: string): string {
  return text
    .split("\n")
    .map((line) => (line in Object.prototype ? WORD_JOINER + line : line))
    .join("\n");
}

/** `option` with the legend's selection: `slots` are the data series, `hidden` those hidden. */
export function withSelection(option: ChartOption, slots: readonly number[], hidden: ReadonlySet<number>): ChartOption {
  const selected = Object.fromEntries(slots.map((slot) => [seriesName(slot), !hidden.has(slot)]));
  return { ...option, legend: { show: false, selected } };
}

/** Lines show their points as dots only while there are few enough to tell apart. */
const MAX_POINTS_WITH_SYMBOLS = 60;

function base(environment: ChartEnvironment, description: string): ChartOption {
  const { theme, animate } = environment;
  return {
    animation: animate,
    animationDuration: 300,
    animationDurationUpdate: 200,
    animationEasing: "cubicOut",
    animationEasingUpdate: "cubicOut",
    // Wall-clock times are kept in UTC fields (format.ts), so the axes read them as UTC.
    useUTC: true,
    textStyle: { fontFamily: theme.font, color: theme.muted, fontSize: 12 },
    aria: { enabled: true, label: { description } },
  };
}

function thresholdLabel(threshold: Threshold, format: DashboardFormat, unit: string | undefined): string {
  return `${threshold.title} ${format.withUnit(format.number(threshold.value), threshold.unit ?? unit)}`;
}

function thresholdSeries(
  thresholds: readonly Threshold[],
  theme: ChartTheme,
  format: DashboardFormat,
  unit: string | undefined,
  /** An x the axis already has (the first time, or category 0), where the invisible points sit. */
  anchor: number,
): ChartOption[] {
  if (thresholds.length === 0) return [];
  return [
    {
      type: "line",
      name: THRESHOLD_SERIES,
      data: thresholds.map((threshold) => [anchor, threshold.value]),
      showSymbol: false,
      lineStyle: { opacity: 0 },
      silent: true,
      legendHoverLink: false,
      // Off the tooltip and the axis pointer: these points are not data.
      tooltip: { show: false },
      emphasis: { disabled: true },
      markLine: {
        silent: true,
        symbol: "none",
        animation: false,
        lineStyle: { color: theme.muted, type: "dashed", width: 1 },
        label: { color: theme.muted, position: "insideEndTop", fontSize: 11 },
        data: thresholds.map((threshold) => ({
          yAxis: threshold.value,
          label: { formatter: () => chartText(thresholdLabel(threshold, format, unit)) },
        })),
      },
    },
  ];
}

/** `scale: false` keeps zero on the axis (bars start at it); ECharts rounds the ends to ticks. */
function valueAxis(theme: ChartTheme, format: DashboardFormat, scale: boolean) {
  return {
    type: "value",
    scale,
    axisLabel: { color: theme.muted, formatter: (value: number) => chartText(format.axisNumber(value)) },
    axisLine: { show: false },
    axisTick: { show: false },
    splitLine: { lineStyle: { color: theme.grid } },
    // The pointer's height is a position, not a value: no label for it.
    axisPointer: { label: { show: false } },
  };
}

function showsSymbols(series: SeriesView, spark: boolean): boolean {
  return !spark && series.points.length <= MAX_POINTS_WITH_SYMBOLS;
}

function lineSeries(series: SeriesView, theme: ChartTheme, area: boolean, spark: boolean): ChartOption {
  const color = theme.series[series.slot] ?? theme.text;
  return {
    type: "line",
    name: seriesName(series.slot),
    data: series.points.map((point) => [point.x, point.y]),
    showSymbol: showsSymbols(series, spark),
    symbol: "circle",
    symbolSize: 7,
    sampling: "lttb",
    connectNulls: false,
    lineStyle: { width: spark ? 1.5 : 2, color },
    itemStyle: { color, borderColor: theme.surface, borderWidth: 2 },
    areaStyle: area ? { color, opacity: spark ? 0.1 : 0.14 } : undefined,
    emphasis: { focus: "none", scale: 1.3 },
  };
}

/**
 * A value with no value on either side draws no line, so while a line hides its dots it would not
 * show at all: this helper series, under the same name (the legend hides both), draws a dot for
 * each such value. It comes after every data series, so a series' index stays its slot.
 */
function loneValues(series: SeriesView, theme: ChartTheme, spark: boolean): ChartOption[] {
  if (showsSymbols(series, spark)) return [];
  const { points } = series;
  const lone = points.filter(
    (point, index) =>
      point.y !== null && (points[index - 1]?.y ?? null) === null && (points[index + 1]?.y ?? null) === null,
  );
  if (lone.length === 0) return [];
  const color = theme.series[series.slot] ?? theme.text;
  return [
    {
      type: "line",
      name: seriesName(series.slot),
      data: lone.map((point) => [point.x, point.y]),
      symbol: "circle",
      symbolSize: spark ? 4 : 5,
      showSymbol: true,
      lineStyle: { opacity: 0 },
      itemStyle: { color },
      silent: true,
      legendHoverLink: false,
      tooltip: { show: false },
      emphasis: { disabled: true },
    },
  ];
}

/** The smallest step between two consecutive times, or `null` with fewer than two. */
function minStep(xs: readonly number[]): number | null {
  let step: number | null = null;
  for (let index = 1; index < xs.length; index++) {
    const gap = (xs[index] as number) - (xs[index - 1] as number);
    if (gap > 0 && (step === null || gap < step)) step = gap;
  }
  return step;
}

export function timeSeriesOption(
  view: TimeSeriesView,
  format: DashboardFormat,
  environment: ChartEnvironment,
  description: string,
): ChartOption {
  const { theme, finePointer } = environment;
  const first = view.xs[0] ?? 0;
  const last = view.xs[view.xs.length - 1] ?? first;
  const span = last - first;
  const step = minStep(view.xs);
  return {
    ...base(environment, description),
    grid: { left: 8, right: 16, top: 24, bottom: 8, containLabel: true },
    tooltip: {
      trigger: "axis",
      showContent: false,
      triggerOn: "mousemove|click",
      axisPointer: {
        type: "cross",
        snap: true,
        lineStyle: { color: theme.muted, type: "dashed", width: 1 },
        crossStyle: { color: theme.muted, type: "dashed", width: 1 },
      },
    },
    xAxis: {
      type: "time",
      min: first,
      max: last,
      // The axis stays at the bottom when values go below zero, with its labels.
      axisLine: { onZero: false, lineStyle: { color: theme.axis } },
      axisTick: { lineStyle: { color: theme.axis } },
      axisLabel: {
        color: theme.muted,
        hideOverlap: true,
        formatter: (value: number) => chartText(format.axisTime(value, span)),
      },
      splitLine: { show: false },
      // No label on the crosshair: zoomed in past every point, the pointer cannot snap to one and
      // its label would show a time no row has. The tooltip names the point's time.
      axisPointer: { label: { show: false } },
    },
    yAxis: valueAxis(theme, format, true),
    dataZoom: [
      {
        type: "inside",
        xAxisIndex: 0,
        filterMode: "none",
        // Zooming in stops at two steps between points, before the window can fall between them.
        minValueSpan: step === null ? undefined : 2 * step,
        zoomOnMouseWheel: true,
        moveOnMouseWheel: false,
        // A mouse drags out a range to zoom into (InteractiveChart); a finger pans.
        moveOnMouseMove: !finePointer,
        // Never cancel a finger's move: the chart's `touch-action: pan-y` leaves vertical swipes
        // to the page, and cancelling them would trap the page's scroll on a phone (sideways
        // drags have no browser default to cancel).
        preventDefaultMouseMove: false,
      },
    ],
    series: [
      ...view.series.map((series) => lineSeries(series, theme, view.mark === "area", false)),
      ...view.series.flatMap((series) => loneValues(series, theme, false)),
      ...thresholdSeries(view.thresholds, theme, format, view.unit, first),
    ],
  };
}

export function sparkOption(series: SeriesView, environment: ChartEnvironment, description: string): ChartOption {
  const { theme } = environment;
  const xs = series.points.map((point) => point.x);
  return {
    ...base(environment, description),
    grid: { left: 4, right: 4, top: 6, bottom: 6, containLabel: false },
    tooltip: {
      trigger: "axis",
      showContent: false,
      triggerOn: "mousemove|click",
      axisPointer: { type: "line", lineStyle: { color: theme.muted, type: "dashed", width: 1 } },
    },
    xAxis: { type: "time", show: false, min: xs[0], max: xs[xs.length - 1] },
    yAxis: { type: "value", show: false, scale: true },
    series: [lineSeries(series, theme, true, true), ...loneValues(series, theme, true)],
  };
}

export function barOption(
  view: BarView,
  format: DashboardFormat,
  environment: ChartEnvironment,
  description: string,
): ChartOption {
  const { theme } = environment;
  const labels = view.categories.map((category) =>
    category.time === null ? category.label : format.wallTime(category.time, view.dateOnly),
  );
  return {
    ...base(environment, description),
    grid: { left: 8, right: 16, top: 24, bottom: 8, containLabel: true },
    tooltip: {
      trigger: "axis",
      showContent: false,
      triggerOn: "mousemove|click",
      // Under the bars (series draw at z 2), so the hovered group is not washed out.
      axisPointer: { type: "shadow", z: 1, shadowStyle: { color: theme.grid, opacity: 0.6 } },
    },
    xAxis: {
      type: "category",
      // Each category by its index; the label is only what the formatter draws.
      data: labels.map((_, index) => String(index)),
      axisLine: { lineStyle: { color: theme.axis } },
      axisTick: { alignWithLabel: true, lineStyle: { color: theme.axis } },
      axisLabel: {
        color: theme.muted,
        hideOverlap: true,
        formatter: (value: string) => chartText(labels[Number(value)] ?? ""),
      },
      axisPointer: { label: { show: false } },
    },
    yAxis: valueAxis(theme, format, false),
    series: [
      ...view.series.map((series) => {
        const color = theme.series[series.slot] ?? theme.text;
        return {
          type: "bar",
          name: seriesName(series.slot),
          data: series.values.map((value) => (value === null ? null : value.y)),
          barMaxWidth: 36,
          barGap: "12%",
          itemStyle: { color, borderRadius: [4, 4, 0, 0] },
          emphasis: { focus: "none" },
        };
      }),
      ...thresholdSeries(view.thresholds, theme, format, view.unit, 0),
    ],
  };
}
