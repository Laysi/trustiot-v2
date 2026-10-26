import { type QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { type ReactNode, useEffect } from "react";
import { RouterProvider } from "react-router/dom";

import { IdentityApiProvider } from "../api/IdentityApiProvider.tsx";
import type { IdentityApi } from "../api/identity.ts";
import { useI18n } from "../i18n/useI18n.ts";
import { PreferencesProvider } from "../preferences/PreferencesProvider.tsx";
import type { PreferencesStore } from "../preferences/preferences.ts";
import { SessionProvider, useSession } from "../session/SessionProvider.tsx";
import { documentTitle } from "../session/session.ts";

export interface AppDependencies {
  queryClient: QueryClient;
  /** The browser's provisional copy of the preferences. */
  preferences: PreferencesStore;
  /** The identity service: the session, its tenant, the preferences and sign-in. */
  identity: IdentityApi;
  /** For tests: the browser's languages, time zone and color-scheme query. */
  languages?: readonly string[];
  browserZone?: string;
  matchMedia?: (query: string) => MediaQueryList;
}

/** The tab's title names the active tenant, so tabs of different tenants can be told apart. */
function DocumentTitle() {
  const { t } = useI18n();
  const { session } = useSession();
  const title = documentTitle(t("app.name"), session?.activeTenant?.name);
  useEffect(() => {
    document.title = title;
  }, [title]);
  return null;
}

/** The providers every page relies on, in dependency order. */
export function AppProviders({ deps, children }: { deps: AppDependencies; children: ReactNode }) {
  return (
    <QueryClientProvider client={deps.queryClient}>
      <IdentityApiProvider api={deps.identity}>
        <SessionProvider source={deps.identity}>
          <PreferencesProvider
            store={deps.preferences}
            api={deps.identity}
            {...(deps.languages ? { languages: deps.languages } : {})}
            {...(deps.browserZone ? { browserZone: deps.browserZone } : {})}
            {...(deps.matchMedia ? { matchMedia: deps.matchMedia } : {})}
          >
            <DocumentTitle />
            {children}
          </PreferencesProvider>
        </SessionProvider>
      </IdentityApiProvider>
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
