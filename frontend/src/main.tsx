import "./styles/global.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter } from "react-router";

import { createApiClient } from "./api/client.ts";
import { deviceApi } from "./api/devices.ts";
import { identityApi } from "./api/identity.ts";
import { App } from "./app/App.tsx";
import { LOGIN_PATH, loginPath } from "./app/paths.ts";
import { reloadOnPreloadError } from "./app/preload-error.ts";
import { createQueryClient } from "./app/query-client.ts";
import { ROUTES } from "./app/routes.tsx";
import {
  applyToDocument,
  browserPreferencesStore,
  DARK_QUERY,
  resolveLocale,
  resolveTheme,
} from "./preferences/preferences.ts";

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

const router = createBrowserRouter(ROUTES);
const queryClient = createQueryClient({
  onUnauthenticated: () => {
    const { pathname, search, hash } = window.location;
    // The sign-in page answers its own 401s (a wrong code); it never sends itself to sign in.
    if (pathname === LOGIN_PATH) return;
    void router.navigate(loginPath(`${pathname}${search}${hash}`), { replace: true });
  },
});

const client = createApiClient();
const root = document.getElementById("root");
if (root === null) {
  throw new Error("index.html has no #root element");
}
createRoot(root).render(
  <StrictMode>
    <App
      router={router}
      deps={{ queryClient, preferences, identity: identityApi(client), devices: deviceApi(client) }}
    />
  </StrictMode>,
);
