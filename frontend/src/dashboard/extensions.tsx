import { createContext, type ReactNode, useContext, useMemo } from "react";

import type { PlatformComponent } from "./spec.ts";

/**
 * What a chart's zoomed stretch is, as the page around it can ask about it: the chart's own title
 * and the edges of the stretch in the chart's x unit (a wall-clock time as the milliseconds of the
 * same fields in UTC; `dateOnly` for a chart of whole days).
 */
export interface ChartStretch {
  title: string;
  from: number;
  to: number;
  dateOnly: boolean;
}

/**
 * What the page that draws components adds to them, without the components knowing what it is
 * (an answer's 「問 AI」 row and 「分析這段」, docs/design/home-ai/design.md). With nothing provided,
 * a component is what the dashboard page draws.
 */
export interface ComponentExtras {
  /** Under a component's content, inside its card (the 「問 AI」 row). */
  footer?(spec: PlatformComponent): ReactNode;
  /** Beside a zoomed chart's reset button: what the page offers for the stretch the zoom shows. */
  stretch?(stretch: ChartStretch): ReactNode;
  /**
   * In the source panel's foot: what the page offers about the source the panel shows (the
   * dashboard page's 「請 AI 解釋」). `title` is the dataset's, computation's or constant's.
   */
  explainSource?(source: { title: string }): ReactNode;
  /**
   * A device's name where the source panel writes a statistics or usage query (an element that
   * reads the name and shows the ID itself while it has none).
   */
  deviceName?(deviceId: string): ReactNode;
}

const NONE: ComponentExtras = {};

const ComponentExtrasContext = createContext<ComponentExtras>(NONE);

/**
 * `extras` for every component below. Give it a stable value: it is a context's. A slot that
 * `extras` leaves out stays what the provider around it gave, so a page can add a slot to what
 * a wider provider already set.
 */
export function ComponentExtrasProvider({ extras, children }: { extras: ComponentExtras; children: ReactNode }) {
  const outer = useContext(ComponentExtrasContext);
  const merged = useMemo(() => ({ ...outer, ...extras }), [outer, extras]);
  return <ComponentExtrasContext.Provider value={merged}>{children}</ComponentExtrasContext.Provider>;
}

export function useComponentExtras(): ComponentExtras {
  return useContext(ComponentExtrasContext);
}
