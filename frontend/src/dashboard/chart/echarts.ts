// ECharts, as the components use it (frontend/README.md, 「儀表板元件的圖表套件」): only the
// modules they need, the canvas renderer, and no part that writes HTML. engine.ts imports this
// module lazily, so it and the library are a chunk of their own.
import { BarChart, LineChart } from "echarts/charts";
import {
  AriaComponent,
  AxisPointerComponent,
  DataZoomInsideComponent,
  GridComponent,
  LegendComponent,
  MarkLineComponent,
  TooltipComponent,
} from "echarts/components";
import { init, use } from "echarts/core";
import { CanvasRenderer } from "echarts/renderers";

import { type ChartInstance, guardRoot } from "./engine.ts";

use([
  LineChart,
  BarChart,
  GridComponent,
  TooltipComponent,
  AxisPointerComponent,
  // Registered for its selection state and the `legendToggleSelect` action; the legend itself
  // is drawn by the components as buttons, and the canvas legend is hidden.
  LegendComponent,
  DataZoomInsideComponent,
  MarkLineComponent,
  AriaComponent,
  CanvasRenderer,
]);

export function createChart(root: HTMLElement): ChartInstance {
  guardRoot(root);
  const chart = init(root, null, { renderer: "canvas" });
  const surface = chart.getZr();
  return {
    setOption: (option, opts) => chart.setOption(option, opts),
    on: (event: string, handler: (payload: never) => void) => {
      chart.on(event, handler as (payload: unknown) => void);
    },
    onSurface: (event, handler) => surface.on(event, handler),
    dispatchAction: (action) => chart.dispatchAction(action as { type: string }),
    xValueAt: (pixelX) => {
      const value = chart.convertFromPixel({ xAxisIndex: 0 }, pixelX) as unknown;
      return typeof value === "number" && Number.isFinite(value) ? value : null;
    },
    inGrid: (pixelX, pixelY) => chart.containPixel({ gridIndex: 0 }, [pixelX, pixelY]),
    resize: () => chart.resize(),
    dispose: () => chart.dispose(),
  } as ChartInstance;
}
