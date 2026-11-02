// A stand-in for ECharts in the component tests (happy-dom has no canvas): it keeps the options and
// actions the component gives it, and the test sends the events ECharts would. A `showTip` action
// answers like ECharts' axis tooltip does: an event with the hit and a position.
import type {
  ChartEngine,
  ChartInstance,
  ChartOption,
  DataZoomEvent,
  ShowTipEvent,
  SurfaceEvent,
} from "../dashboard/chart/engine.ts";

type Handler = (event: never) => void;

export class FakeChart implements ChartInstance {
  readonly options: ChartOption[] = [];
  readonly actions: Record<string, unknown>[] = [];
  disposed = false;
  private readonly handlers = new Map<string, Handler[]>();
  private readonly surface = new Map<string, Handler[]>();

  constructor(
    readonly root: HTMLElement,
    /** Makes `setOption` throw, as ECharts did on a series named `hasOwnProperty`. */
    private readonly setOptionError: Error | null = null,
  ) {}

  get option(): ChartOption {
    const last = this.options[this.options.length - 1];
    if (last === undefined) throw new Error("no option was set");
    return last;
  }

  setOption(option: ChartOption): void {
    if (this.setOptionError !== null) throw this.setOptionError;
    this.options.push(option);
  }

  on(event: string, handler: Handler): void {
    this.handlers.set(event, [...(this.handlers.get(event) ?? []), handler]);
  }

  onSurface(event: string, handler: (event: SurfaceEvent) => void): void {
    this.surface.set(event, [...(this.surface.get(event) ?? []), handler as Handler]);
  }

  dispatchAction(action: Record<string, unknown>): void {
    this.actions.push(action);
    if (action.type === "showTip" && typeof action.seriesIndex === "number" && typeof action.dataIndex === "number") {
      this.showTip(action.seriesIndex, action.dataIndex);
    }
    if (action.type === "hideTip") this.emit("hideTip", {});
  }

  /** What ECharts sends when the axis tooltip points at a data item. */
  showTip(seriesIndex: number, dataIndex: number, x = 100, y = 50): void {
    const event: ShowTipEvent = {
      x,
      y,
      dataByCoordSys: [{ dataByAxis: [{ seriesDataIndices: [{ seriesIndex, dataIndex }] }] }],
    };
    this.emit("showTip", event);
  }

  emit(event: "showTip" | "hideTip" | "datazoom", payload: ShowTipEvent | DataZoomEvent | object): void {
    for (const handler of this.handlers.get(event) ?? []) (handler as (payload: unknown) => void)(payload);
  }

  emitSurface(event: string, payload: SurfaceEvent = { offsetX: 100, offsetY: 50 }): void {
    for (const handler of this.surface.get(event) ?? []) (handler as (payload: SurfaceEvent) => void)(payload);
  }

  /** One millisecond per pixel. */
  xValueAt(pixelX: number): number | null {
    return pixelX;
  }

  inGrid(): boolean {
    return true;
  }

  resize(): void {}

  dispose(): void {
    this.disposed = true;
  }
}

/**
 * An engine that makes `FakeChart`s and remembers them, in order of creation. Like `echarts.init`,
 * it hands an element that already has a chart that same chart until it is disposed.
 */
export function fakeEngine(options: { setOptionError?: Error } = {}): {
  engine: ChartEngine;
  charts: FakeChart[];
  /** How many times a component asked for the library. */
  loads: () => number;
  /** How many times a component created a chart, whether or not it got one it already had. */
  creates: () => number;
} {
  const charts: FakeChart[] = [];
  let loads = 0;
  let creates = 0;
  return {
    charts,
    loads: () => loads,
    creates: () => creates,
    engine: async () => {
      loads++;
      return (root) => {
        creates++;
        const existing = charts.find((chart) => chart.root === root && !chart.disposed);
        if (existing !== undefined) return existing;
        const chart = new FakeChart(root, options.setOptionError ?? null);
        charts.push(chart);
        return chart;
      };
    },
  };
}
