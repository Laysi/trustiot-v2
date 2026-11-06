import { Outlet } from "react-router";

import type { ConversationApi } from "../api/conversations.ts";
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
  options: {
    role?: "viewer" | "editor" | "admin";
    seed?: "normal" | "empty" | "none";
    /** The conversation service the 「問 AI」 buttons ask (the default one answers no question). */
    conversations?: ConversationApi;
  } = {},
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
  return renderAt(path, {
    routes,
    identity: signedInBrowser({ role: options.role ?? "editor" }).browser,
    ...(options.conversations ? { conversations: options.conversations } : {}),
  });
}
