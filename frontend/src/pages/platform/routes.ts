import type { ComponentType } from "react";
import { type RouteObject, redirect } from "react-router";

import { aiRoutes } from "./ai/plugin.ts";
import { PlatformLayout } from "./PlatformLayout.tsx";
import { PLATFORM_PATH, PLATFORM_TENANTS_PATH } from "./paths.ts";

/** A page loaded when its route is first matched, as a chunk of its own (as `app/routes.tsx` does). */
function lazyPage<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

/**
 * The routes of the platform back office: one route, `/platform`, whose element is the layout (the
 * guard for platform administrators, the cross-tenant notice, the section navigation) and whose
 * children are the pages, those of the AI group included (`ai/plugin.ts`). Paths here are relative
 * to the layout. Until the endpoints behind these pages are in `api/openapi.json` they are mounted
 * only in the UI preview (`preview.ts`); when they are, `app/routes.tsx` mounts this route next to
 * `TenantGate`, not under it: the pages must open while the administrator's own tenant is
 * suspended (design.md, 「資訊架構」).
 */
export const platformRoutes: RouteObject[] = [
  {
    path: PLATFORM_PATH.slice(1),
    Component: PlatformLayout,
    children: [
      { index: true, loader: () => redirect(PLATFORM_TENANTS_PATH) },
      { path: "tenants", ...lazyPage(() => import("./TenantsPage.tsx"), "TenantsPage") },
      { path: "tenants/new", ...lazyPage(() => import("./TenantCreatePage.tsx"), "TenantCreatePage") },
      { path: "tenants/:tenantId", ...lazyPage(() => import("./TenantDetailPage.tsx"), "TenantDetailPage") },
      { path: "gateways", ...lazyPage(() => import("./GatewaysPage.tsx"), "GatewaysPage") },
      { path: "provisioning", ...lazyPage(() => import("./ProvisioningPage.tsx"), "ProvisioningPage") },
      { path: "audit-log", ...lazyPage(() => import("./AuditLogPage.tsx"), "AuditLogPage") },
      ...aiRoutes,
    ],
  },
];
