import type { ComponentType } from "react";
import type { RouteObject } from "react-router";

import { ErrorPage } from "../pages/ErrorPage.tsx";
import { LoginPage } from "../pages/LoginPage.tsx";
import { HomePage, NotFoundPage } from "../pages/pages.tsx";
import { PLATFORM_AI_PATH } from "../pages/platform/ai/paths.ts";
import { productionAiPages } from "../pages/platform/ai/plugin.ts";
import { TenantGate } from "../session/TenantGate.tsx";
import { Layout } from "./Layout.tsx";
import { PageLoading } from "./PageLoading.tsx";
import { CONVERSATIONS_PATH, DASHBOARDS_PATH, DEVICES_PATH, LOGIN_PATH, MODELS_PATH, SETTINGS_PATH } from "./paths.ts";

/**
 * A page that is loaded when its route is first matched, as a chunk of its own: it stays out of
 * the entry bundle that every visitor downloads. A chunk that no longer exists after a deployment
 * is the case `reloadOnPreloadError` (preload-error.ts) handles.
 */
function lazyPage<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

/**
 * The route under which every page of a tenant's data sits (`TenantGate`); the UI preview adds the
 * routes of pages whose backend does not exist yet there (frontend/README.md, 「UI 預覽」).
 */
export const TENANT_PAGES_ROUTE_ID = "tenant-pages";

/**
 * Every client route. Each page of MS2 adds its route here; routes.test.ts keeps them out of the
 * paths the ingress does not send to the SPA (paths.ts) and checks that deep links render.
 * Every route has an error boundary, so a failing page never leaves a blank screen.
 *
 * The settings page is about the user and the not-found page shows no data, so both open without
 * an active tenant; every page that works on a tenant's data goes under `TenantGate`, which first lands the
 * session in a tenant and replaces the pages of a suspended one.
 */
export const ROUTES: RouteObject[] = [
  {
    path: LOGIN_PATH,
    element: <LoginPage />,
    errorElement: <ErrorPage />,
  },
  {
    path: "/",
    element: <Layout />,
    // Shown while the code of a lazily loaded page is on its way, on a first visit to that page.
    HydrateFallback: PageLoading,
    errorElement: <ErrorPage />,
    children: [
      {
        errorElement: <ErrorPage />,
        children: [
          { path: SETTINGS_PATH.slice(1), ...lazyPage(() => import("../pages/SettingsPage.tsx"), "SettingsPage") },
          {
            // The platform administration works across tenants, so it is outside `TenantGate`: a
            // suspended active tenant must not hide the pages that can restore it. Only the pages
            // whose API exists are here; the others are in pages/platform/ai/preview.ts.
            path: PLATFORM_AI_PATH.slice(1),
            lazy: async () => ({ Component: (await import("../pages/platform/ai/AiShell.tsx")).AiShell }),
            children: productionAiPages,
          },
          {
            id: TENANT_PAGES_ROUTE_ID,
            element: <TenantGate />,
            children: [
              { index: true, element: <HomePage /> },
              {
                path: CONVERSATIONS_PATH.slice(1),
                ...lazyPage(() => import("../pages/conversations/ConversationsPage.tsx"), "ConversationsPage"),
              },
              {
                path: `${CONVERSATIONS_PATH.slice(1)}/:conversationId`,
                ...lazyPage(() => import("../pages/conversations/ConversationPage.tsx"), "ConversationPage"),
              },
              {
                path: DASHBOARDS_PATH.slice(1),
                ...lazyPage(() => import("../pages/dashboards/DashboardsPage.tsx"), "DashboardsPage"),
              },
              {
                path: `${DASHBOARDS_PATH.slice(1)}/:dashboardId`,
                ...lazyPage(() => import("../pages/dashboards/DashboardPage.tsx"), "DashboardPage"),
              },
              {
                path: DEVICES_PATH.slice(1),
                ...lazyPage(() => import("../pages/device/DevicesPage.tsx"), "DevicesPage"),
              },
              // The static `new` outranks `:deviceId`, and `settings` is one segment deeper.
              {
                path: `${DEVICES_PATH.slice(1)}/new`,
                ...lazyPage(() => import("../pages/device/DeviceNewPage.tsx"), "DeviceNewPage"),
              },
              {
                path: `${DEVICES_PATH.slice(1)}/:deviceId/settings`,
                ...lazyPage(() => import("../pages/device/DeviceSettingsPage.tsx"), "DeviceSettingsPage"),
              },
              {
                path: `${DEVICES_PATH.slice(1)}/:deviceId`,
                ...lazyPage(() => import("../pages/device/DevicePage.tsx"), "DevicePage"),
              },
              {
                path: MODELS_PATH.slice(1),
                ...lazyPage(() => import("../pages/models/ModelsPage.tsx"), "ModelsPage"),
              },
              {
                path: `${MODELS_PATH.slice(1)}/new`,
                ...lazyPage(() => import("../pages/models/ModelEditPage.tsx"), "ModelNewPage"),
              },
              {
                path: `${MODELS_PATH.slice(1)}/:modelId`,
                ...lazyPage(() => import("../pages/models/ModelEditPage.tsx"), "ModelPage"),
              },
            ],
          },
          { path: "*", element: <NotFoundPage /> },
        ],
      },
    ],
  },
];
