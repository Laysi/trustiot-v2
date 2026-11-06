import { createContext, type ReactNode, useContext } from "react";

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
}

const NONE: ComponentExtras = {};

const ComponentExtrasContext = createContext<ComponentExtras>(NONE);

/** `extras` for every component below. Give it a stable value: it is a context's. */
export function ComponentExtrasProvider({ extras, children }: { extras: ComponentExtras; children: ReactNode }) {
  return <ComponentExtrasContext.Provider value={extras}>{children}</ComponentExtrasContext.Provider>;
}

export function useComponentExtras(): ComponentExtras {
  return useContext(ComponentExtrasContext);
}
