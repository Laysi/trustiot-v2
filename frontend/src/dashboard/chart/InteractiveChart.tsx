import { RotateCcw } from "lucide-react";
import { type KeyboardEvent, type ReactNode, useContext, useEffect, useLayoutEffect, useRef, useState } from "react";

import { Button, focusRing } from "../../components/ui/button.tsx";
import { useMediaQuery } from "../../lib/use-media-query.ts";
import { cn } from "../../lib/utils.ts";
import { useDashboardEnv, useThemeVersion } from "../context.tsx";
import {
  ChartEngineContext,
  type ChartInstance,
  type ChartOption,
  type ChartTheme,
  type DataZoomEvent,
  readTheme,
  type ShowTipEvent,
} from "./engine.ts";
import { seriesName, withSelection } from "./options.ts";

/** A data item ECharts knows: the series (in option order) and the index in its data. */
export interface ChartHit {
  seriesIndex: number;
  dataIndex: number;
}

export interface ChartEnvironment {
  theme: ChartTheme;
  /** False under `prefers-reduced-motion: reduce`: no entry or update transitions. */
  animate: boolean;
  /** A mouse or pen (`pointer: fine`): dragging selects a range; on touch it pans instead. */
  finePointer: boolean;
}

export interface InteractiveChartProps {
  /** Names the chart for assistive technology. */
  label: string;
  size: "chart" | "spark";
  /** The option for the data, without the legend's selection (the chart adds it). */
  buildOption(environment: ChartEnvironment): ChartOption;
  /**
   * The positions the keyboard steps through, in order (a time or a category). The tooltip, the
   * slider's value text and the source all speak of a stop.
   */
  stopCount: number;
  /**
   * What names a stop across new data: its time (the x value) or its category, never its index.
   * Data that arrives while a stop is selected keeps the selection on the stop with the same key;
   * when no stop has it any more, the tooltip closes. `undefined` for a stop out of range.
   */
  stopKey(stop: number): StopKey | undefined;
  /** A visible item at `stop`, for showing its tooltip from the keyboard. */
  hitOf(stop: number): ChartHit | null;
  /** The stop an item ECharts points at belongs to. */
  stopOf(hit: ChartHit): number | null;
  /** The stop as one line of text (the slider's value text). */
  describe(stop: number): string;
  renderTip(stop: number): ReactNode;
  onSource(stop: number): void;
  /** The color slots of the option's data series, each named `seriesName(slot)` there. */
  slots: readonly number[];
  /** The slots the legend buttons hide. */
  hiddenSlots: ReadonlySet<number>;
  /** Wheel, pinch and drag-to-select zoom on the x axis (time series only). */
  zoom: boolean;
  /**
   * Told the visible x window (the option's x unit) whenever the zoom changes, and `null` when it
   * is back to the full range. The device page recomputes its figures for the window.
   */
  onZoom?(window: ZoomWindow | null): void;
  /** Shown beside the reset button while the chart is zoomed (the device page's 「set as range」). */
  zoomActions?(window: ZoomWindow): ReactNode;
}

/** What names a stop across new data (`InteractiveChartProps.stopKey`). */
export type StopKey = string | number;

/** The x values at the edges of the visible part of a zoomed chart. */
export interface ZoomWindow {
  from: number;
  to: number;
}

/** Pixels a drag has to cover before it selects a range instead of being a click. */
const MIN_BRUSH_PX = 6;
/** Pixels between the pointer and the tooltip. */
const TIP_GAP_PX = 12;

interface Selection {
  stop: number;
  key: StopKey;
}

function selectionOf(stop: number, props: InteractiveChartProps): Selection | null {
  const key = props.stopKey(stop);
  return key === undefined ? null : { stop, key };
}

/**
 * Where a selection is in the current data: the stop it was made on while that still has its key,
 * else the stop that has the key now, else `null` (the time or category left the data).
 */
function resolveStop(selection: Selection, props: InteractiveChartProps): number | null {
  const { stop, key } = selection;
  if (stop < props.stopCount && props.stopKey(stop) === key) return stop;
  for (let candidate = 0; candidate < props.stopCount; candidate++) {
    if (props.stopKey(candidate) === key) return candidate;
  }
  return null;
}

/** Whether rendering `node` draws nothing, so a box around it would be empty. */
function drawsNothing(node: ReactNode): boolean {
  return node === null || node === undefined || typeof node === "boolean" || node === "";
}

function hitsOf(event: ShowTipEvent): ChartHit[] {
  const hits: ChartHit[] = [];
  for (const system of event.dataByCoordSys ?? []) {
    for (const axis of system.dataByAxis ?? []) {
      for (const item of axis.seriesDataIndices ?? [])
        hits.push({ seriesIndex: item.seriesIndex, dataIndex: item.dataIndex });
    }
  }
  if (hits.length === 0 && event.seriesIndex !== undefined && event.dataIndex !== undefined) {
    hits.push({ seriesIndex: event.seriesIndex, dataIndex: event.dataIndex });
  }
  return hits;
}

function zoomedBy(event: DataZoomEvent): boolean {
  const range = event.batch?.[0] ?? event;
  if (typeof range.start === "number" && typeof range.end === "number") return range.start > 0.01 || range.end < 99.99;
  return true;
}

/**
 * One ECharts chart with the interactions every dashboard chart shares
 * (docs/architecture/ai-assistant.md, 「元件模式」): ECharts draws the marks and the crosshair on a
 * canvas; the tooltip is ours (`showContent: false`, driven by `showTip` and `hideTip`), so
 * every number in it comes from the component's formatter; clicking a point, or pressing Enter,
 * opens its source. For assistive technology the chart is a slider over its stops, like the
 * device page's chart: the arrow keys move the selection and show its tooltip, and the value
 * text reads the stop out.
 */
export function InteractiveChart(props: InteractiveChartProps) {
  const { t } = useDashboardEnv();
  const engine = useContext(ChartEngineContext);
  const animate = !useMediaQuery("(prefers-reduced-motion: reduce)");
  const finePointer = useMediaQuery("(pointer: fine)");
  const themeVersion = useThemeVersion();

  const frame = useRef<HTMLDivElement>(null);
  const root = useRef<HTMLDivElement>(null);
  const tip = useRef<HTMLDivElement>(null);
  const brushBox = useRef<HTMLDivElement>(null);
  const chart = useRef<ChartInstance | null>(null);
  const [ready, setReady] = useState(false);
  const [failed, setFailed] = useState(false);
  // The selected stop is kept with its key, and its index is looked up in the current data on each
  // render: new data moves a selection to the same time (or category), or clears it.
  const [selected, setSelected] = useState<Selection | null>(null);
  const [tipAt, setTipAt] = useState<{ x: number; y: number } | null>(null);
  const [zoomWindow, setZoomWindow] = useState<ZoomWindow | null>(null);
  const [brush, setBrush] = useState<{ from: number; to: number } | null>(null);

  const active = selected === null ? null : resolveStop(selected, props);
  // The handlers ECharts calls are registered once; they read the latest props and state here.
  const latest = useRef({ props, active, selected, finePointer });
  latest.current = { props, active, selected, finePointer };
  const brushStart = useRef<number | null>(null);
  const skipClick = useRef(false);
  const pointerFocus = useRef(false);

  useEffect(() => {
    const element = root.current;
    if (element === null) return;
    let disposed = false;
    let instance: ChartInstance | null = null;
    engine().then(
      (create) => {
        // Only a mount that is still current creates its chart (engine.ts, `ChartEngine`).
        if (disposed) return;
        let created: ChartInstance;
        try {
          created = create(element);
        } catch {
          setFailed(true);
          return;
        }
        instance = created;
        chart.current = created;
        created.on("showTip", (event) => {
          const { props: current } = latest.current;
          const stop = hitsOf(event)
            .map((hit) => current.stopOf(hit))
            .find((candidate): candidate is number => candidate !== null);
          if (stop === undefined) return;
          const next = selectionOf(stop, current);
          if (next === null) return;
          setSelected(next);
          if (typeof event.x === "number" && typeof event.y === "number") setTipAt({ x: event.x, y: event.y });
        });
        created.on("hideTip", () => {
          if (document.activeElement !== frame.current) setSelected(null);
        });
        const followZoom = (zoomedNow: boolean) => {
          // A zoom the library cannot read back still shows the reset button (an empty window).
          const visible = zoomedNow ? (created.xWindow() ?? { from: Number.NaN, to: Number.NaN }) : null;
          setZoomWindow(visible);
          latest.current.props.onZoom?.(visible !== null && Number.isFinite(visible.from) ? visible : null);
        };
        created.on("datazoom", (event) => followZoom(zoomedBy(event)));
        created.onSurface("click", () => {
          if (skipClick.current) {
            skipClick.current = false;
            return;
          }
          const stop = latest.current.active;
          if (stop !== null) latest.current.props.onSource(stop);
        });
        created.onSurface("mousedown", (event) => {
          // The renderer drops the click after a drag that moved the pointer, so a new press
          // clears the skip instead of the next click.
          skipClick.current = false;
          const { props: current, finePointer: fine } = latest.current;
          if (!current.zoom || !fine || !created.inGrid(event.offsetX, event.offsetY)) return;
          brushStart.current = event.offsetX;
        });
        created.onSurface("mousemove", (event) => {
          const from = brushStart.current;
          if (from === null) return;
          if (Math.abs(event.offsetX - from) >= MIN_BRUSH_PX) setBrush({ from, to: event.offsetX });
        });
        created.onSurface("mouseup", (event) => {
          const from = brushStart.current;
          brushStart.current = null;
          setBrush(null);
          if (from === null || Math.abs(event.offsetX - from) < MIN_BRUSH_PX) return;
          const a = created.xValueAt(from);
          const b = created.xValueAt(event.offsetX);
          if (a === null || b === null) return;
          skipClick.current = true;
          created.dispatchAction({ type: "dataZoom", startValue: Math.min(a, b), endValue: Math.max(a, b) });
          followZoom(true);
        });
        created.onSurface("globalout", () => {
          brushStart.current = null;
          setBrush(null);
        });
        setReady(true);
      },
      () => {
        if (!disposed) setFailed(true);
      },
    );
    return () => {
      disposed = true;
      instance?.dispose();
      chart.current = null;
      setReady(false);
    };
  }, [engine]);

  const { buildOption, slots, hiddenSlots } = props;
  const hiddenNow = useRef(hiddenSlots);
  hiddenNow.current = hiddenSlots;

  // A new option replaces the old one (new data, theme or motion setting): the zoom starts over,
  // the legend's selection is kept.
  // biome-ignore lint/correctness/useExhaustiveDependencies: themeVersion re-reads the theme's colors
  useEffect(() => {
    const instance = chart.current;
    const element = root.current;
    if (!ready || instance === null || element === null) return;
    const option = buildOption({ theme: readTheme(element), animate, finePointer });
    instance.setOption(withSelection(option, slots, hiddenNow.current), { notMerge: true });
    setZoomWindow(null);
    latest.current.props.onZoom?.(null);
  }, [ready, buildOption, slots, animate, finePointer, themeVersion]);

  // The legend buttons toggle series without replacing the option, so the zoom stays.
  useEffect(() => {
    const instance = chart.current;
    if (!ready || instance === null) return;
    for (const slot of slots) {
      const type = hiddenSlots.has(slot) ? "legendUnSelect" : "legendSelect";
      instance.dispatchAction({ type, name: seriesName(slot) });
    }
  }, [ready, hiddenSlots, slots]);

  // The replaced option has lost ECharts' tooltip and crosshair (and the legend actions above ran
  // after it), so the selected stop is shown again where the new data puts it: the tooltip follows
  // the point, not the old pointer position (the `showTip` action's event gives the position). A
  // stop whose time or category left the data closes the tooltip. Same dependencies as the option's
  // effect, declared after it, so it runs exactly when an option was replaced.
  // biome-ignore lint/correctness/useExhaustiveDependencies: see above
  useEffect(() => {
    const instance = chart.current;
    const { props: current, selected: previous } = latest.current;
    if (!ready || instance === null || previous === null) return;
    const stop = resolveStop(previous, current);
    if (stop === null) {
      setSelected(null);
      instance.dispatchAction({ type: "hideTip" });
      return;
    }
    const hit = current.hitOf(stop);
    if (hit === null) {
      setTipAt(null);
      return;
    }
    setSelected({ stop, key: previous.key });
    instance.dispatchAction({ type: "showTip", ...hit });
  }, [ready, buildOption, slots, animate, finePointer, themeVersion]);

  useEffect(() => {
    const element = root.current;
    if (!ready || element === null || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => chart.current?.resize());
    observer.observe(element);
    return () => observer.disconnect();
  }, [ready]);

  // Position the tooltip and the selection box through the CSSOM: the CSP allows no style attribute.
  useLayoutEffect(() => {
    const box = tip.current;
    const container = frame.current;
    if (box === null || container === null || tipAt === null) return;
    const width = container.clientWidth;
    const boxWidth = box.offsetWidth;
    // Beside the pointer, on the roomier side first; where neither side has room (a phone), as
    // close to the pointer as the chart's edges allow.
    const right = tipAt.x + TIP_GAP_PX;
    const left = tipAt.x - TIP_GAP_PX - boxWidth;
    const sides = tipAt.x > width / 2 ? [left, right] : [right, left];
    const fitting = sides.find((x) => x >= 0 && x + boxWidth <= width);
    box.style.left = `${fitting ?? Math.max(0, Math.min(width - boxWidth, sides[0] ?? 0))}px`;
    const height = box.offsetHeight;
    box.style.top = `${Math.max(0, Math.min(tipAt.y - height / 2, container.clientHeight - height))}px`;
  });
  useLayoutEffect(() => {
    const box = brushBox.current;
    if (box === null || brush === null) return;
    box.style.left = `${Math.min(brush.from, brush.to)}px`;
    box.style.width = `${Math.abs(brush.to - brush.from)}px`;
  });

  const show = (stop: number) => {
    setSelected(selectionOf(stop, props));
    const hit = props.hitOf(stop);
    if (hit !== null) chart.current?.dispatchAction({ type: "showTip", ...hit });
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const count = props.stopCount;
    if (count === 0) return;
    const last = count - 1;
    const current = active ?? last;
    const page = Math.max(1, Math.round(count / 10));
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      props.onSource(current);
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      setSelected(null);
      chart.current?.dispatchAction({ type: "hideTip" });
      return;
    }
    const next: Record<string, number> = {
      ArrowLeft: Math.max(0, current - 1),
      ArrowDown: Math.max(0, current - 1),
      ArrowRight: Math.min(last, current + 1),
      ArrowUp: Math.min(last, current + 1),
      PageDown: Math.max(0, current - page),
      PageUp: Math.min(last, current + page),
      Home: 0,
      End: last,
    };
    const target = next[event.key];
    if (target === undefined) return;
    event.preventDefault();
    show(target);
  };

  const valueIndex = active ?? props.stopCount - 1;
  const tipContent = active === null || tipAt === null ? null : props.renderTip(active);
  const spark = props.size === "spark";
  return (
    <div className="grid min-w-0 gap-2">
      {props.zoom && !spark ? (
        <div className="flex min-h-8 flex-wrap items-center justify-between gap-2">
          <p className="text-xs text-muted-foreground">
            {t(finePointer ? "dashboard.chart.zoom_hint" : "dashboard.chart.zoom_hint_touch")}
          </p>
          {zoomWindow !== null ? (
            <div className="flex flex-wrap items-center gap-2">
              {props.zoomActions && Number.isFinite(zoomWindow.from) ? props.zoomActions(zoomWindow) : null}
              <Button
                size="sm"
                variant="outline"
                onClick={() => chart.current?.dispatchAction({ type: "dataZoom", start: 0, end: 100 })}
              >
                <RotateCcw aria-hidden="true" />
                {t("dashboard.chart.reset_zoom")}
              </Button>
            </div>
          ) : null}
        </div>
      ) : null}
      <div
        ref={frame}
        role="slider"
        tabIndex={0}
        aria-label={props.label}
        aria-orientation="horizontal"
        aria-valuemin={0}
        aria-valuemax={Math.max(0, props.stopCount - 1)}
        aria-valuenow={Math.max(0, valueIndex)}
        aria-valuetext={props.stopCount > 0 ? props.describe(Math.max(0, valueIndex)) : props.label}
        onKeyDown={onKeyDown}
        onPointerDown={() => {
          pointerFocus.current = true;
        }}
        onFocus={() => {
          // Focus from the keyboard selects the newest stop; a click selects what it points at.
          if (!pointerFocus.current && active === null && props.stopCount > 0) show(props.stopCount - 1);
          pointerFocus.current = false;
        }}
        onBlur={() => {
          setSelected(null);
          chart.current?.dispatchAction({ type: "hideTip" });
        }}
        className={cn("relative w-full min-w-0 rounded-md", focusRing)}
      >
        <div
          ref={root}
          aria-hidden="true"
          data-testid="chart-root"
          className={cn("w-full touch-pan-y select-none", spark ? "h-14" : "h-72")}
        />
        {brush !== null ? (
          <div
            ref={brushBox}
            aria-hidden="true"
            className="pointer-events-none absolute inset-y-2 rounded-sm border border-primary/60 bg-primary/10"
          />
        ) : null}
        {!drawsNothing(tipContent) ? (
          <div
            ref={tip}
            aria-hidden="true"
            data-testid="chart-tooltip"
            className="pointer-events-none absolute z-10 w-max max-w-64 rounded-md border bg-popover px-3 py-2 text-xs text-popover-foreground shadow-md"
          >
            {tipContent}
          </div>
        ) : null}
        {failed ? <p className="text-sm text-destructive">{t("dashboard.chart.failed")}</p> : null}
      </div>
    </div>
  );
}
