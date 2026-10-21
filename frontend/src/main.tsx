import "./styles/global.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter } from "react-router";

import { App } from "./app/App.tsx";
import { loginPath } from "./app/paths.ts";
import { reloadOnPreloadError } from "./app/preload-error.ts";
import { createQueryClient } from "./app/query-client.ts";
import { ROUTES } from "./app/routes.tsx";
import { browserPreferencesStore } from "./preferences/preferences.ts";
import { noSessionSource } from "./session/session.ts";

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

const router = createBrowserRouter(ROUTES);
const queryClient = createQueryClient({
  onUnauthenticated: () => {
    const { pathname, search, hash } = window.location;
    void router.navigate(loginPath(`${pathname}${search}${hash}`), { replace: true });
  },
});

const root = document.getElementById("root");
if (root === null) {
  throw new Error("index.html has no #root element");
}
createRoot(root).render(
  <StrictMode>
    <App
      router={router}
      deps={{
        queryClient,
        preferences: browserPreferencesStore(localStorageOrUndefined(), navigator.languages),
        session: noSessionSource,
      }}
    />
  </StrictMode>,
);
