import type { RouteObject } from "react-router";

import { DevicePage } from "../pages/device/DevicePage.tsx";
import { ErrorPage } from "../pages/ErrorPage.tsx";
import { LoginPage } from "../pages/LoginPage.tsx";
import { HomePage, NotFoundPage } from "../pages/pages.tsx";
import { SettingsPage } from "../pages/SettingsPage.tsx";
import { TenantGate } from "../session/TenantGate.tsx";
import { Layout } from "./Layout.tsx";
import { DEVICES_PATH, LOGIN_PATH, SETTINGS_PATH } from "./paths.ts";

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
    errorElement: <ErrorPage />,
    children: [
      {
        errorElement: <ErrorPage />,
        children: [
          { path: SETTINGS_PATH.slice(1), element: <SettingsPage /> },
          {
            element: <TenantGate />,
            children: [
              { index: true, element: <HomePage /> },
              { path: `${DEVICES_PATH.slice(1)}/:deviceId`, element: <DevicePage /> },
            ],
          },
          { path: "*", element: <NotFoundPage /> },
        ],
      },
    ],
  },
];
