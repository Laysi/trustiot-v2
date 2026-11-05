import type { ComponentType } from "react";
import type { RouteObject } from "react-router";

import { aiRoutes } from "./ai/plugin.ts";
import { platformLayoutRoute } from "./layout-route.ts";

/** A page loaded when its route is first matched, as a chunk of its own (as `app/routes.tsx` does). */
function lazyPage<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

/**
 * The back office's own pages (tenants, gateways, provisioning, audit log), relative to
 * `/platform`. Their endpoints are not all in `api/openapi.json` yet, so only the UI preview
 * mounts them (`preview.ts`); when they are, they join `productPlatformRoutes`
 * (`layout-route.ts`), which `app/routes.tsx` mounts next to `TenantGate`, not under it: the pages
 * must open while the administrator's own tenant is suspended (design.md, 「資訊架構」).
 */
export const platformPages: RouteObject[] = [
  { path: "tenants", ...lazyPage(() => import("./TenantsPage.tsx"), "TenantsPage") },
  { path: "tenants/new", ...lazyPage(() => import("./TenantCreatePage.tsx"), "TenantCreatePage") },
  { path: "tenants/:tenantId", ...lazyPage(() => import("./TenantDetailPage.tsx"), "TenantDetailPage") },
  { path: "gateways", ...lazyPage(() => import("./GatewaysPage.tsx"), "GatewaysPage") },
  { path: "provisioning", ...lazyPage(() => import("./ProvisioningPage.tsx"), "ProvisioningPage") },
  { path: "audit-log", ...lazyPage(() => import("./AuditLogPage.tsx"), "AuditLogPage") },
];

/**
 * The whole back office as one `/platform` route, as it will be mounted once every endpoint
 * exists: the layout with these pages and the AI group's production pages. The tests render it
 * (`src/test/render-platform.tsx`).
 */
export const platformRoutes: RouteObject[] = [platformLayoutRoute([...platformPages, ...aiRoutes])];
