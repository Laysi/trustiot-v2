import type { RouteObject } from "react-router";

import type { MenuItem } from "../../app/menu.ts";

/**
 * The generic home, for the UI preview only. Its data (status counts, usage, anomalies and device
 * health) has no API yet (Laysi/trustiot-v2#453, #118), so the production routes do not list it;
 * the home page's own switch to this layout waits for those endpoints and for an AI availability
 * signal. It is not a menu entry: in the design it is the home, not a page of the menu.
 */
export const previewRoutes: RouteObject[] = [
  {
    path: "overview",
    lazy: async () => ({ Component: (await import("./OverviewPage.tsx")).OverviewPage }),
  },
];

export const previewMenu: MenuItem[] = [];
