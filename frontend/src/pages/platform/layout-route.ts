import type { RouteObject } from "react-router";

import { aiRoutes } from "./ai/plugin.ts";
import { PLATFORM_PATH } from "./paths.ts";

/**
 * The `/platform` route: the layout (`PlatformLayout`: the guard for platform administrators, the
 * cross-tenant notice, the section navigation) with `/platform` itself going to the first section
 * the build has, and `children` (paths relative to `/platform`) as its pages. The layout is loaded
 * with its first page, as a chunk of its own.
 */
export function platformLayoutRoute(children: RouteObject[]): RouteObject {
  return {
    path: PLATFORM_PATH.slice(1),
    lazy: async () => ({ Component: (await import("./PlatformLayout.tsx")).PlatformLayout }),
    children: [
      { index: true, lazy: async () => ({ Component: (await import("./PlatformLayout.tsx")).PlatformIndex }) },
      ...children,
    ],
  };
}

/**
 * The back office in the product: only the pages whose API is in `api/openapi.json`, which today
 * are the AI group's model roles (`ai/plugin.ts`). `app/routes.tsx` mounts it next to `TenantGate`,
 * not under it: the pages must open while the administrator's own tenant is suspended (design.md,
 * 「資訊架構」). The pages whose API is pending are mounted by the UI preview (`preview.ts`).
 */
export const productPlatformRoutes: RouteObject[] = [platformLayoutRoute(aiRoutes)];
