import { BellIcon } from "lucide-react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../app/menu.ts";
import { ALERTS_PATH } from "./paths.ts";

// Only in the UI preview until the alerts API exists (Laysi/trustiot-v2#168, #169): the pages read
// api/pending/alerts.ts. When the API is in api/openapi.json these move into routes.tsx and menu.ts
// (frontend/README.md, 「後端完成時要改的地方」).

const under = (path: string, pathname: string) => pathname === path || pathname.startsWith(`${path}/`);

export const previewRoutes: RouteObject[] = [
  {
    path: ALERTS_PATH.slice(1),
    lazy: async () => ({ Component: (await import("./AlertsPage.tsx")).AlertsPage }),
  },
  {
    path: `${ALERTS_PATH.slice(1)}/notifications/:notificationId`,
    lazy: async () => ({ Component: (await import("./AlertsPage.tsx")).AlertsPage }),
  },
  {
    path: `${ALERTS_PATH.slice(1)}/rules/:ruleId`,
    lazy: async () => ({ Component: (await import("./RuleDetailPage.tsx")).RuleDetailPage }),
  },
];

export const previewMenu: MenuItem[] = [
  {
    id: "alerts",
    group: 3,
    label: "shell.menu.alerts",
    icon: BellIcon,
    path: ALERTS_PATH,
    covers: (pathname) => under(ALERTS_PATH, pathname),
    // Every member of a tenant reads alerts (design 「角色」); only the buttons depend on the role.
    visibleTo: (session) => session.tenants.length > 0,
  },
];
