import {
  type KeyboardEvent,
  type PointerEvent,
  type RefObject,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import { focusRing } from "../../components/ui/button.tsx";
import { cn } from "../../lib/utils.ts";
import {
  bandPath,
  type Datum,
  type Frame,
  linePath,
  nearestIndex,
  scales,
  segments,
  timeTicks,
  valueAxis,
} from "./chart.ts";

/** Before the first measurement, and where nothing can be measured (tests). */
const FALLBACK_WIDTH = 640;
const HEIGHT = 260;
/** Roughly one label's width per tick on the time axis. */
const PX_PER_TIME_TICK = 110;
/** An estimate of one digit's width in the axis font (text-xs), to fit the value labels. */
const CHAR_PX = 7;

/** The width of `element`, kept up to date as the layout changes. */
function useWidth(element: RefObject<HTMLElement | null>): number {
  const [width, setWidth] = useState(FALLBACK_WIDTH);
  useLayoutEffect(() => {
    const target = element.current;
    if (!target) return;
    const measure = () => {
      if (target.clientWidth > 0) setWidth(target.clientWidth);
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(target);
    return () => observer.disconnect();
  }, [element]);
  return width;
}

export interface TimeSeriesChartProps {
  data: readonly Datum[];
  /** The range the answer covers: the axis spans it even where there are no buckets. */
  from: number;
  to: number;
  /** The bucket length, which tells a missing bucket from the usual spacing. */
  intervalMs: number | undefined;
  kind: "number" | "bool";
  timeZone: string;
  /** Names the chart for assistive technology, with how to use it. */
  label: string;
  formatValue(value: number): string;
  formatTick(time: number, step: number): string;
  /** The text for one bucket, shown above the chart while it is pointed at or selected. */
  describe(datum: Datum): string;
  /** Shown above the chart while no bucket is selected. */
  hint: string;
}

/**
 * A line chart of one data point's buckets, drawn as SVG: the mean as a line, the band between
 * each bucket's minimum and maximum behind it, breaks where buckets are missing. No chart library:
 * every shape is an SVG attribute and every color a class, so nothing needs an inline style the
 * CSP refuses (frontend/README.md, 「設備頁」).
 *
 * Pointing at the chart, or focusing it and pressing the arrow keys, selects a bucket, and the line
 * above the chart shows its values. For assistive technology the chart is a slider over the
 * buckets whose value text is that line, so each arrow key reads the next bucket out; the drawing
 * itself is hidden, and the page's table of the same buckets is the full accessible version.
 */
export function TimeSeriesChart(props: TimeSeriesChartProps) {
  const { data, from, to, intervalMs, kind, timeZone } = props;
  const container = useRef<HTMLDivElement>(null);
  const svg = useRef<SVGSVGElement>(null);
  const width = useWidth(container);
  const [active, setActive] = useState<number | null>(null);

  const geometry = useMemo(() => {
    const axis = valueAxis(data, kind);
    const labels = axis.ticks.map((tick) => props.formatValue(tick));
    const left = Math.max(...labels.map((label) => label.length)) * CHAR_PX + 16;
    const frame: Frame = { left, right: width - 12, top: 10, bottom: HEIGHT - 28 };
    const scale = scales(frame, { from, to }, axis);
    const time = timeTicks(from, to, Math.floor((frame.right - frame.left) / PX_PER_TIME_TICK), timeZone);
    const runs = segments(data, intervalMs);
    return {
      frame,
      scale,
      yTicks: axis.ticks.map((tick, index) => ({ value: tick, label: labels[index] ?? "" })),
      xTicks: time.ticks.map((tick) => ({ time: tick, label: props.formatTick(tick, time.step) })),
      lines: runs.filter((run) => run.length > 1).map((run) => linePath(run, scale)),
      bands: runs.map((run) => bandPath(run, scale)).filter((path): path is string => path !== null),
      dots: runs.filter((run) => run.length === 1).map((run) => run[0] as Datum),
    };
  }, [data, kind, width, from, to, intervalMs, timeZone, props.formatValue, props.formatTick]);

  const { frame, scale } = geometry;
  const selected = active === null ? undefined : data[active];

  const onPointerMove = (event: PointerEvent<SVGSVGElement>) => {
    const box = svg.current?.getBoundingClientRect();
    if (!box || data.length === 0) return;
    setActive(nearestIndex(data, scale.timeAt(event.clientX - box.left)));
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (data.length === 0) return;
    const last = data.length - 1;
    const current = active ?? last;
    const page = Math.max(1, Math.round(data.length / 10));
    const next: Record<string, number | null> = {
      ArrowLeft: Math.max(0, current - 1),
      ArrowDown: Math.max(0, current - 1),
      ArrowRight: Math.min(last, current + 1),
      ArrowUp: Math.min(last, current + 1),
      PageDown: Math.max(0, current - page),
      PageUp: Math.min(last, current + page),
      Home: 0,
      End: last,
      Escape: null,
    };
    if (!(event.key in next)) return;
    event.preventDefault();
    setActive(next[event.key] ?? null);
  };

  // The slider's value is the selected bucket; with none selected, the newest one.
  const valueIndex = active ?? data.length - 1;
  const valueDatum = data[valueIndex];
  return (
    <div className="grid gap-2">
      <p aria-hidden="true" className="min-h-5 text-sm tabular-nums">
        {selected ? props.describe(selected) : <span className="text-muted-foreground">{props.hint}</span>}
      </p>
      <div
        ref={container}
        role="slider"
        tabIndex={0}
        aria-label={props.label}
        aria-orientation="horizontal"
        aria-valuemin={0}
        aria-valuemax={Math.max(0, data.length - 1)}
        aria-valuenow={Math.max(0, valueIndex)}
        aria-valuetext={valueDatum ? props.describe(valueDatum) : props.hint}
        onKeyDown={onKeyDown}
        onFocus={() => setActive((current) => current ?? (data.length > 0 ? data.length - 1 : null))}
        onBlur={() => setActive(null)}
        className={cn("w-full rounded-md", focusRing)}
      >
        <svg
          ref={svg}
          width={width}
          height={HEIGHT}
          aria-hidden="true"
          data-testid="history-chart"
          className="block touch-pan-y select-none"
          onPointerMove={onPointerMove}
          onPointerDown={onPointerMove}
          onPointerLeave={() => setActive(null)}
        >
          {geometry.yTicks.map((tick) => (
            <g key={`y${tick.value}`}>
              <line
                x1={frame.left}
                x2={frame.right}
                y1={scale.y(tick.value)}
                y2={scale.y(tick.value)}
                className="stroke-border"
              />
              <text
                x={frame.left - 8}
                y={scale.y(tick.value)}
                textAnchor="end"
                dominantBaseline="middle"
                className="fill-muted-foreground text-xs tabular-nums"
              >
                {tick.label}
              </text>
            </g>
          ))}
          {geometry.xTicks.map((tick) => (
            <g key={`x${tick.time}`}>
              <line
                x1={scale.x(tick.time)}
                x2={scale.x(tick.time)}
                y1={frame.bottom}
                y2={frame.bottom + 4}
                className="stroke-muted-foreground"
              />
              <text
                x={scale.x(tick.time)}
                y={frame.bottom + 18}
                textAnchor="middle"
                className="fill-muted-foreground text-xs tabular-nums"
              >
                {tick.label}
              </text>
            </g>
          ))}
          {geometry.bands.map((path) => (
            <path key={path} d={path} className="fill-primary/15 stroke-none" />
          ))}
          {geometry.lines.map((path) => (
            <path
              key={path}
              d={path}
              data-testid="history-line"
              className="fill-none stroke-primary"
              strokeWidth={2}
              strokeLinejoin="round"
              strokeLinecap="round"
            />
          ))}
          {geometry.dots.map((datum) => (
            <circle
              key={datum.time}
              data-testid="history-dot"
              cx={scale.x(datum.time)}
              cy={scale.y(datum.value)}
              r={3}
              className="fill-primary"
            />
          ))}
          {selected ? (
            <g>
              <line
                x1={scale.x(selected.time)}
                x2={scale.x(selected.time)}
                y1={frame.top}
                y2={frame.bottom}
                strokeDasharray="4 4"
                className="stroke-foreground/50"
              />
              <circle
                cx={scale.x(selected.time)}
                cy={scale.y(selected.value)}
                r={4.5}
                strokeWidth={2}
                className="fill-background stroke-primary"
              />
            </g>
          ) : null}
        </svg>
      </div>
    </div>
  );
}
