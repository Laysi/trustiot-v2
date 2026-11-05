import { FileTextIcon } from "lucide-react";
import type { ComponentType } from "react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../app/menu.ts";
import { REPORTS_PATH } from "./paths.ts";

/**
 * The reports pages live only in the UI preview until the report API exists (Laysi/trustiot-v2#173):
 * their routes and their menu entry are not in app/routes.tsx and app/menu.ts, so the product
 * never shows them. `src/test/fake-reports.ts` is the API they read in the preview. When the
 * backend lands, both move into routes.tsx and menu.ts (frontend/README.md, 「UI 預覽」).
 */

/** A page loaded when its route is first matched, as the product's routes do. */
function page<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

export const previewRoutes: RouteObject[] = [
  { path: REPORTS_PATH.slice(1), ...page(() => import("./ReportsPage.tsx"), "ReportsPage") },
  {
    path: `${REPORTS_PATH.slice(1)}/schedules/new`,
    ...page(() => import("./ScheduleFormPage.tsx"), "ScheduleFormPage"),
  },
  {
    path: `${REPORTS_PATH.slice(1)}/schedules/:scheduleId/edit`,
    ...page(() => import("./ScheduleFormPage.tsx"), "ScheduleFormPage"),
  },
  { path: `${REPORTS_PATH.slice(1)}/:reportId`, ...page(() => import("./ReportPage.tsx"), "ReportPage") },
];

export const previewMenu: MenuItem[] = [
  {
    id: "reports",
    group: 3,
    label: "shell.menu.reports",
    icon: FileTextIcon,
    path: REPORTS_PATH,
    covers: (pathname) => pathname === REPORTS_PATH || pathname.startsWith(`${REPORTS_PATH}/`),
    // Every member of the tenant; the plan's wall is the page's.
    visibleTo: (session) => session.tenants.length > 0,
  },
];
