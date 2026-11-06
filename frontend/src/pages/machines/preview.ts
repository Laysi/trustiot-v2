import { FactoryIcon } from "lucide-react";
import type { ComponentType } from "react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../app/menu.ts";
import { MACHINES_PATH } from "../../app/paths.ts";

/**
 * The machine pages are only in the UI preview until the field-context service and its API exist
 * (Laysi/trustiot-v2#177; frontend/README.md, 「UI 預覽」): when they do, these move into
 * `app/routes.tsx` and `app/menu.ts`, and this file goes.
 */
function lazyPage<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

export const previewRoutes: RouteObject[] = [
  { path: MACHINES_PATH.slice(1), ...lazyPage(() => import("./MachinesPage.tsx"), "MachinesPage") },
  { path: `${MACHINES_PATH.slice(1)}/:machineId`, ...lazyPage(() => import("./MachinePage.tsx"), "MachinePage") },
];

export const previewMenu: MenuItem[] = [
  {
    id: "machines",
    group: 3,
    label: "shell.menu.machines",
    icon: FactoryIcon,
    path: MACHINES_PATH,
    covers: (pathname) => pathname === MACHINES_PATH || pathname.startsWith(`${MACHINES_PATH}/`),
    // The machines of the tenant the session works in.
    visibleTo: (session) => session.tenants.length > 0,
  },
];
