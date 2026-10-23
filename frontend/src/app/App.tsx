import { type QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { RouterProvider } from "react-router/dom";
import { PreferencesProvider } from "../preferences/PreferencesProvider.tsx";
import type { PreferencesStore } from "../preferences/preferences.ts";
import { SessionProvider } from "../session/SessionProvider.tsx";
import type { SessionSource } from "../session/session.ts";

export interface AppDependencies {
  queryClient: QueryClient;
  preferences: PreferencesStore;
  session: SessionSource;
  /** For tests: the browser's time zone and color-scheme query. */
  browserZone?: string;
  matchMedia?: (query: string) => MediaQueryList;
}

/** The providers every page relies on, in dependency order. */
export function AppProviders({ deps, children }: { deps: AppDependencies; children: ReactNode }) {
  return (
    <QueryClientProvider client={deps.queryClient}>
      <PreferencesProvider
        store={deps.preferences}
        {...(deps.browserZone ? { browserZone: deps.browserZone } : {})}
        {...(deps.matchMedia ? { matchMedia: deps.matchMedia } : {})}
      >
        <SessionProvider source={deps.session}>{children}</SessionProvider>
      </PreferencesProvider>
    </QueryClientProvider>
  );
}

export function App({
  deps,
  router,
}: {
  deps: AppDependencies;
  router: Parameters<typeof RouterProvider>[0]["router"];
}) {
  return (
    <AppProviders deps={deps}>
      <RouterProvider router={router} />
    </AppProviders>
  );
}
