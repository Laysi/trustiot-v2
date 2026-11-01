// The part of ECharts the components use, behind an interface: the real one is loaded lazily
// (echarts.ts, its own chunk, so the first page does not load the library), and tests pass a fake
// through `ChartEngineContext` (happy-dom has no canvas). Only this module and echarts.ts know
// ECharts is underneath; the components hand it options and read its events.
import { createContext } from "react";

/** An option object as ECharts takes it; built by the components (options.ts). */
export type ChartOption = Record<string, unknown>;

/** What a `showTip` event says, for an axis tooltip or one set off with a series and data index. */
export interface ShowTipEvent {
  x?: number;
  y?: number;
  seriesIndex?: number;
  dataIndex?: number;
  dataByCoordSys?: ReadonlyArray<{
    dataByAxis?: ReadonlyArray<{ seriesDataIndices?: ReadonlyArray<{ seriesIndex: number; dataIndex: number }> }>;
  }>;
}

/** A pointer event of the chart's drawing surface, in pixels from the chart's top left. */
export interface SurfaceEvent {
  offsetX: number;
  offsetY: number;
}

export interface DataZoomEvent {
  start?: number;
  end?: number;
  batch?: ReadonlyArray<{ start?: number; end?: number }>;
}

export interface ChartInstance {
  setOption(option: ChartOption, opts?: { notMerge?: boolean }): void;
  on(event: "showTip", handler: (event: ShowTipEvent) => void): void;
  on(event: "hideTip", handler: () => void): void;
  on(event: "datazoom", handler: (event: DataZoomEvent) => void): void;
  onSurface(
    event: "click" | "mousedown" | "mousemove" | "mouseup" | "globalout",
    handler: (event: SurfaceEvent) => void,
  ): void;
  dispatchAction(action: Record<string, unknown>): void;
  /** The x value (time in ms) at a pixel column of the first grid; `null` when there is none. */
  xValueAt(pixelX: number): number | null;
  /** Whether a pixel is inside the first grid (the plot area). */
  inGrid(pixelX: number, pixelY: number): boolean;
  resize(): void;
  dispose(): void;
}

/** Creates a chart in `root`, which it fills. */
export type ChartFactory = (root: HTMLElement) => ChartInstance;

/**
 * Loads the chart library and resolves to its factory. Loading and creating are two steps so that
 * a component creates its chart only while it is still mounted: under React's StrictMode (and on
 * a quick remount) two loads can resolve after the first mount was cleaned up, and ECharts hands
 * a second `init` on the same element the first chart, which the first mount's cleanup then
 * disposes.
 */
export type ChartEngine = () => Promise<ChartFactory>;

/** The real engine, loaded on first use. */
export const loadEcharts: ChartEngine = async () => (await import("./echarts.ts")).createChart;

export const ChartEngineContext = createContext<ChartEngine>(loadEcharts);

/**
 * zrender empties its root with `root.innerHTML = ""`, a Trusted Types sink that fails once the
 * policy is enforced (frontend/README.md, 「儀表板元件的圖表套件」). The root's own `innerHTML`
 * setter clears it with `replaceChildren()` instead and refuses any other string, so no HTML from
 * the library ever reaches the DOM through it.
 */
export function guardRoot(root: HTMLElement): void {
  const native = Object.getOwnPropertyDescriptor(Element.prototype, "innerHTML");
  Object.defineProperty(root, "innerHTML", {
    configurable: true,
    get: () => native?.get?.call(root) ?? "",
    set: (value: unknown) => {
      if (value === "" || value === null) {
        root.replaceChildren();
        return;
      }
      throw new Error("the chart root accepts no HTML");
    },
  });
}

/** The colors and font a chart draws with, read from the theme's custom properties. */
export interface ChartTheme {
  series: readonly string[];
  text: string;
  muted: string;
  grid: string;
  axis: string;
  surface: string;
  font: string;
}

export const SERIES_TOKENS = ["--chart-1", "--chart-2", "--chart-3", "--chart-4", "--chart-5", "--chart-6"] as const;

/**
 * A CSS color as `rgba()`: the canvas understands oklch, but zrender parses colors itself to
 * blend them (hover, opacity) and knows only the older forms. A 1×1 canvas does the conversion,
 * as the browser computes it.
 */
function toRgba(color: string, probe: CanvasRenderingContext2D | null): string {
  if (probe === null || color === "") return color;
  probe.clearRect(0, 0, 1, 1);
  probe.fillStyle = "#000000";
  probe.fillStyle = color;
  probe.fillRect(0, 0, 1, 1);
  const [r, g, b, a] = probe.getImageData(0, 0, 1, 1).data;
  return `rgba(${r}, ${g}, ${b}, ${((a ?? 255) / 255).toFixed(3)})`;
}

/** The theme as it applies to `element` now. */
export function readTheme(element: Element): ChartTheme {
  const style = getComputedStyle(element);
  let probe: CanvasRenderingContext2D | null = null;
  try {
    const canvas = document.createElement("canvas");
    canvas.width = 1;
    canvas.height = 1;
    probe = canvas.getContext("2d", { willReadFrequently: true });
  } catch {
    probe = null;
  }
  const token = (name: string) => toRgba(style.getPropertyValue(name).trim(), probe);
  return {
    series: SERIES_TOKENS.map(token),
    text: token("--foreground"),
    muted: token("--muted-foreground"),
    grid: token("--border"),
    axis: token("--input"),
    surface: token("--card"),
    font: style.fontFamily || "sans-serif",
  };
}
