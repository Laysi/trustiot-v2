import { ShieldIcon } from "lucide-react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../app/menu.ts";
import { PLATFORM_PATH, PLATFORM_TENANTS_PATH } from "./paths.ts";
import { platformRoutes } from "./routes.ts";
import { under } from "./sections.ts";

/**
 * The platform back office exists only in the UI preview for now: its tenant list, usage figures,
 * invitation, quota, health overview and unbinding, repair and decommissioning endpoints are not in
 * `api/openapi.json` (the list is in `src/api/pending/platform.ts`), so the pages run on the fake
 * `src/test/fake-platform.ts`. They move to `app/routes.tsx` and `app/menu.ts` when those exist.
 */
export const previewRoutes: RouteObject[] = platformRoutes;

export const previewMenu: MenuItem[] = [
  {
    id: "platform",
    group: 5,
    label: "shell.menu.platform",
    icon: ShieldIcon,
    path: PLATFORM_TENANTS_PATH,
    covers: (pathname) => under(PLATFORM_PATH, pathname),
    // Platform administrators only; the API refuses everyone else with 403.
    visibleTo: (session) => session.user.platformAdmin,
  },
];
