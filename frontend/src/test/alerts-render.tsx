import { Outlet } from "react-router";

import { AlertsApiProvider } from "../api/pending/AlertsApiProvider.tsx";
import { ROUTES } from "../app/routes.tsx";
import { previewRoutes } from "../pages/alerts/preview.ts";
import { withRoutes } from "../ui-preview/collect.ts";
import { type FakeAlertsServer, seedAlerts } from "./fake-alerts.ts";
import { renderAt, signedInBrowser } from "./render.tsx";

/**
 * Renders the alerts pages the way the UI preview mounts them: the product's routes plus
 * `previewRoutes` under the tenant's pages, with the fake as the alerts API, signed in with `role`.
 */
export function renderAlerts(
  path: string,
  server: FakeAlertsServer,
  options: { role?: "viewer" | "editor" | "admin"; seed?: "normal" | "empty" | "none" } = {},
) {
  if (options.seed !== "none") seedAlerts(server, options.seed ?? "normal");
  const routes = withRoutes(ROUTES, [
    {
      element: (
        <AlertsApiProvider api={server}>
          <Outlet />
        </AlertsApiProvider>
      ),
      children: previewRoutes,
    },
  ]);
  return renderAt(path, { routes, identity: signedInBrowser({ role: options.role ?? "editor" }).browser });
}
