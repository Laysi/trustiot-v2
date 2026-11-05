import { type ReactElement, StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter, type RouteObject } from "react-router";

import { assetsApi } from "../api/assets.ts";
import { createApiClient } from "../api/client.ts";
import { conversationApi } from "../api/conversations.ts";
import { dashboardApi } from "../api/dashboards.ts";
import { deviceApi } from "../api/devices.ts";
import { identityApi } from "../api/identity.ts";
import { modelRoleApi } from "../api/modelRoles.ts";
import {
  applyToDocument,
  browserPreferencesStore,
  DARK_QUERY,
  resolveLocale,
  resolveTheme,
} from "../preferences/preferences.ts";
import { shownSession } from "../session/SessionProvider.tsx";
import { browserSessionChannel } from "../session/session-channel.ts";
import { App } from "./App.tsx";
import { LOGIN_PATH, loginPath } from "./paths.ts";
import { reloadOnPreloadError } from "./preload-error.ts";
import { createQueryClient } from "./query-client.ts";

function sessionStorageOrUndefined(): Storage | undefined {
  try {
    return window.sessionStorage;
  } catch {
    return undefined;
  }
}

function localStorageOrUndefined(): Storage | undefined {
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}

/** How `startApp` is set up; main.tsx gives only the routes. */
export interface StartOptions {
  routes: RouteObject[];
  /** Where the API client sends its requests; the network unless given (the UI preview gives its fakes). */
  fetch?: (request: Request) => Promise<Response>;
  /** Wraps the app in more providers (the UI preview's fakes of APIs that do not exist yet). */
  wrap?: (app: ReactElement) => ReactElement;
}

/**
 * Starts the SPA in the page's #root: the browser's preferences, the router, the query client and
 * the API client. main.tsx starts it for the product; the UI preview (src/ui-preview/) starts the
 * same app with its fakes, so there is one copy of the app.
 */
export function startApp(options: StartOptions): void {
  reloadOnPreloadError({
    target: window,
    storage: sessionStorageOrUndefined(),
    reload: () => window.location.reload(),
    now: () => Date.now(),
  });

  // The browser's copy of the preferences is applied before the first render, so the page does not
  // flash the wrong theme or language while the server's values load.
  const preferences = browserPreferencesStore(localStorageOrUndefined());
  const cached = preferences.load();
  applyToDocument(
    document.documentElement,
    resolveLocale(cached.language, navigator.languages),
    resolveTheme(cached.theme, window.matchMedia?.(DARK_QUERY).matches ?? false),
  );

  const router = createBrowserRouter(options.routes);
  const queryClient = createQueryClient({
    onUnauthenticated: () => {
      const { pathname, search, hash } = window.location;
      // The sign-in page answers its own 401s (a wrong code); it never sends itself to sign in.
      if (pathname === LOGIN_PATH) return;
      void router.navigate(loginPath(`${pathname}${search}${hash}`), { replace: true });
    },
  });

  // Every request names the user and the tenant the pages show, so the cookie check refuses it once
  // another tab has signed in as someone else or moved the session elsewhere (Laysi/trustiot-v2#678).
  const client = createApiClient({
    shownSession: () => shownSession(queryClient),
    ...(options.fetch ? { fetch: options.fetch } : {}),
  });
  const root = document.getElementById("root");
  if (root === null) {
    throw new Error("the page has no #root element");
  }
  const app = (
    <App
      router={router}
      deps={{
        queryClient,
        preferences,
        identity: identityApi(client),
        devices: deviceApi(client),
        assets: assetsApi(client),
        dashboards: dashboardApi(client),
        conversations: conversationApi(client),
        modelRoles: modelRoleApi(client),
        sessionChannel: browserSessionChannel(),
      }}
    />
  );
  createRoot(root).render(<StrictMode>{options.wrap ? options.wrap(app) : app}</StrictMode>);
}
