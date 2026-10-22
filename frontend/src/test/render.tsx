import { QueryClient } from "@tanstack/react-query";
import { render } from "@testing-library/react";
import { createMemoryRouter, type RouteObject } from "react-router";

import { App, type AppDependencies } from "../app/App.tsx";
import type { Preferences, PreferencesStore } from "../preferences/preferences.ts";
import { noSessionSource, type SessionSource } from "../session/session.ts";

/** Preferences held in memory; `saved` lists every save. */
export function memoryPreferences(initial: Preferences): PreferencesStore & { saved: Preferences[] } {
  let current = initial;
  const saved: Preferences[] = [];
  return {
    saved,
    load: () => current,
    save: (preferences) => {
      current = preferences;
      saved.push(preferences);
    },
  };
}

/** A media query that never matches dark and never changes. */
export function lightMedia(): (query: string) => MediaQueryList {
  return (query) =>
    ({
      matches: false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    }) as unknown as MediaQueryList;
}

/** Renders the app's providers and a memory router at `path`, as a reload of that URL would. */
export function renderAt(
  path: string,
  options: {
    routes: RouteObject[];
    preferences?: PreferencesStore;
    session?: SessionSource;
    queryClient?: QueryClient;
  },
) {
  const router = createMemoryRouter(options.routes, { initialEntries: [path] });
  const deps: AppDependencies = {
    queryClient: options.queryClient ?? new QueryClient({ defaultOptions: { queries: { retry: false } } }),
    preferences: options.preferences ?? memoryPreferences({ locale: "en", timeZone: "UTC", theme: "system" }),
    session: options.session ?? noSessionSource,
    browserZone: "UTC",
    matchMedia: lightMedia(),
  };
  return { router, deps, ...render(<App router={router} deps={deps} />) };
}
