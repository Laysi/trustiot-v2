import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createContext, type ReactNode, useCallback, useContext, useEffect, useMemo } from "react";

import { useI18n } from "../i18n/useI18n.ts";
import { documentTitle, type SessionInfo, type SessionSource } from "./session.ts";

export const SESSION_QUERY_KEY = ["session"] as const;

export interface SessionContextValue {
  session: SessionInfo | null;
  loading: boolean;
  /**
   * Switches the active tenant. Everything cached belonged to the previous tenant: requests still
   * running are cancelled and every other query loses its data, so no page shows the previous
   * tenant's data while the new tenant's loads.
   */
  switchTenant(tenantId: string): Promise<void>;
}

const SessionContext = createContext<SessionContextValue | null>(null);

export function SessionProvider({ source, children }: { source: SessionSource; children: ReactNode }) {
  const queryClient = useQueryClient();
  const { t } = useI18n();
  const query = useQuery({
    queryKey: SESSION_QUERY_KEY,
    queryFn: ({ signal }) => source.load(signal),
    staleTime: 60_000,
  });
  const session = query.data ?? null;
  const tenantName = session?.activeTenant?.name;
  const appName = t("app.name");

  useEffect(() => {
    document.title = documentTitle(appName, tenantName);
  }, [appName, tenantName]);

  const switchTenant = useCallback(
    async (tenantId: string) => {
      const next = await source.switchTenant(tenantId);
      await queryClient.cancelQueries();
      queryClient.setQueryData(SESSION_QUERY_KEY, next);
      // Reset, not remove: a page that is open keeps observing its query and refetches it for the
      // new tenant. Refetch errors reach the query cache's handler like any other.
      void queryClient.resetQueries({ predicate: (query) => query.queryKey[0] !== SESSION_QUERY_KEY[0] });
    },
    [queryClient, source],
  );

  const value = useMemo<SessionContextValue>(
    () => ({ session, loading: query.isPending, switchTenant }),
    [session, query.isPending, switchTenant],
  );
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession(): SessionContextValue {
  const value = useContext(SessionContext);
  if (value === null) {
    throw new Error("useSession needs a SessionProvider");
  }
  return value;
}
