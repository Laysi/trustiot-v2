import { ShieldIcon } from "lucide-react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../app/menu.ts";
import { previewAiPages } from "./ai/preview.ts";
import { PlatformLayout } from "./PlatformLayout.tsx";
import { PLATFORM_PATH, PLATFORM_TENANTS_PATH, under } from "./paths.ts";
import { platformPages } from "./routes.ts";

/**
 * The platform back office's pages whose API is pending exist only in the UI preview: its tenant
 * list, usage figures, invitation, quota, health overview and unbinding, repair and
 * decommissioning endpoints are not in `api/openapi.json` (the list is in
 * `src/api/pending/platform.ts`), and neither are the AI group's system prompts and usage
 * (`ai/preview.ts`), so the pages run on fakes. They move to `productPlatformRoutes`
 * (`layout-route.ts`) and `app/menu.ts` when those exist.
 *
 * The product already mounts `/platform` (the layout, `/platform` itself and the model roles,
 * next to `TenantGate`), and the preview adds its pages under `TenantGate`. So these pages get a
 * layout of their own that has no path, and their paths are written in full: no URL is defined by
 * both, and each page still sits in the same layout and section navigation. There is deliberately
 * no index route here: `/platform` itself is the product's index route (`PlatformIndex` in
 * `layout-route.ts`, which the preview's routes include because they start from `app/routes.tsx`),
 * and it goes to the first section the build has. In the preview `fake-platform.ts`'s provider
 * makes the tenants available, so `/platform` lands there (`preview.test.tsx` checks it); a second
 * index route here would define `/platform` twice.
 */
function inPlatform(pages: readonly RouteObject[]): RouteObject[] {
  return pages.map((page) => ({ ...page, path: `${PLATFORM_PATH.slice(1)}/${page.path ?? ""}` }) as RouteObject);
}

export const previewRoutes: RouteObject[] = [
  { Component: PlatformLayout, children: inPlatform([...platformPages, ...previewAiPages]) },
];

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
