import { RouterIcon } from "lucide-react";
import type { RouteObject } from "react-router";
import type { MenuItem } from "../../app/menu.ts";
import { GATEWAYS_PATH } from "../../app/paths.ts";

/**
 * The gateway page is only in the UI preview: its binding and health endpoints do not exist yet
 * (api/pending/gateways.ts). When they do, both move into `app/routes.tsx` and `app/menu.ts`
 * (frontend/README.md, 「UI 預覽」, 「後端完成時要改的地方」).
 */
export const previewRoutes: RouteObject[] = [
  {
    path: GATEWAYS_PATH.slice(1),
    lazy: async () => ({ Component: (await import("./GatewaysPage.tsx")).GatewaysPage }),
  },
];

export const previewMenu: MenuItem[] = [
  {
    id: "gateways",
    // Group 2 with the devices (docs/design/app-shell.md): every member of a tenant.
    group: 2,
    label: "shell.menu.gateways",
    icon: RouterIcon,
    path: GATEWAYS_PATH,
    covers: (pathname) => pathname === GATEWAYS_PATH || pathname.startsWith(`${GATEWAYS_PATH}/`),
    visibleTo: (session) => session.tenants.length > 0,
  },
];
