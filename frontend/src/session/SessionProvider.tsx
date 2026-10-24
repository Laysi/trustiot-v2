import { type QueryKey, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { createContext, type ReactNode, useCallback, useContext, useEffect, useMemo, useState } from "react";

import { classifyError } from "../api/errors.ts";
import type { RequestMeta } from "../app/query-client.ts";
import type { SessionInfo, SessionSource } from "./session.ts";

export const SESSION_QUERY_KEY = ["session"] as const;
export const PREFERENCES_QUERY_KEY = ["preferences"] as const;

/** Queries about the user, not the tenant: a tenant switch keeps them. */
const USER_QUERIES: ReadonlySet<unknown> = new Set([SESSION_QUERY_KEY[0], PREFERENCES_QUERY_KEY[0]]);

export function isUserQuery(key: QueryKey): boolean {
  return USER_QUERIES.has(key[0]);
}

export type SessionStatus = "loading" | "signed_in" | "signed_out" | "error";

export interface SessionContextValue {
  status: SessionStatus;
  /** Set while signed in. */
  session: SessionInfo | null;
  /** Why the session could not be read, when `status` is "error". */
  error: unknown;
  /** Reads the session again. */
  reload(): void;
  /**
   * Switches the active tenant. Everything cached for the tenant belonged to the previous one:
   * its requests still running are cancelled and every tenant query loses its data, so no page
   * shows the previous tenant's data while the new tenant's loads.
   */
  switchTenant(tenantId: string): Promise<void>;
  /** Whether a request of the active tenant was refused because the tenant is suspended. */
  suspended: boolean;
  /** Ends this browser's session, or every session of the user. */
  endSession(scope: "this" | "everywhere"): Promise<void>;
  /** After the session ended: forgets everything cached for the user, then shows them signed out. */
  forget(): Promise<void>;
  /**
   * After signing in: forgets anything cached before and reads the new session. Resolves with the
   * session `session` will hold once this provider renders again (the cache tells its observers
   * on a later tick), so a caller can wait for that before showing a signed-in page.
   */
  signedIn(): Promise<SessionInfo | null>;
}

const SessionContext = createContext<SessionContextValue | null>(null);

/**
 * The signed-in user and their tenants, from the identity service. Every page lives under it; the
 * signed-in pages are behind `Layout`, which sends a signed-out visitor to sign in.
 */
export function SessionProvider({ source, children }: { source: SessionSource; children: ReactNode }) {
  const queryClient = useQueryClient();
  const query = useQuery({
    queryKey: SESSION_QUERY_KEY,
    queryFn: ({ signal }) => source.loadSession(signal),
    staleTime: 60_000,
  });
  const session = query.data ?? null;
  const status: SessionStatus = query.isPending
    ? "loading"
    : query.isError && query.data === undefined
      ? "error"
      : session
        ? "signed_in"
        : "signed_out";

  const [suspendedTenant, setSuspendedTenant] = useState<string | null>(null);
  const activeTenant = session?.activeTenant?.id ?? null;

  const { mutateAsync: switchOnServer } = useMutation({
    mutationFn: (tenantId: string) => source.switchTenant(tenantId),
    meta: { switchesTenant: true },
  });

  const switchTenant = useCallback(
    async (tenantId: string) => {
      const next = await switchOnServer(tenantId);
      // The preferences are the user's and stay valid; cancelling their first read would leave it
      // pending for good (nothing refetches a query that never goes stale). The session is
      // cancelled too: a read that started before the switch must not replace its answer.
      await queryClient.cancelQueries({ predicate: (cached) => cached.queryKey[0] !== PREFERENCES_QUERY_KEY[0] });
      queryClient.setQueryData(SESSION_QUERY_KEY, next);
      setSuspendedTenant(null);
      // Reset, not remove: a page that is open keeps observing its query and refetches it for the
      // new tenant. Refetch errors reach the query cache's handler like any other.
      void queryClient.resetQueries({ predicate: (cached) => !isUserQuery(cached.queryKey) });
    },
    [queryClient, switchOnServer],
  );

  const forgetCached = useCallback(async () => {
    await queryClient.cancelQueries();
    queryClient.removeQueries({ predicate: (cached) => cached.queryKey[0] !== SESSION_QUERY_KEY[0] });
    setSuspendedTenant(null);
  }, [queryClient]);

  const endSession = useCallback(
    (scope: "this" | "everywhere") => (scope === "this" ? source.logOut() : source.logOutEverywhere()),
    [source],
  );

  const forget = useCallback(async () => {
    await forgetCached();
    queryClient.setQueryData(SESSION_QUERY_KEY, null);
  }, [forgetCached, queryClient]);

  const signedIn = useCallback(async () => {
    await forgetCached();
    await queryClient.fetchQuery({
      queryKey: SESSION_QUERY_KEY,
      queryFn: ({ signal }) => source.loadSession(signal),
      staleTime: 0,
    });
    // The cached value, which is what `session` becomes: fetchQuery resolves with the answer
    // itself, and the cache keeps the previous object when the answer equals it.
    return queryClient.getQueryData<SessionInfo | null>(SESSION_QUERY_KEY) ?? null;
  }, [forgetCached, queryClient, source]);

  // What a refused request says about the session, whichever request the page made. A 401 means
  // the session ended (signed out on another device, expired): the user's cached data goes, and
  // the signed-in layout sends them to sign in. A request that answers its own 401s (the sign-in
  // form) says so in its meta. tenant_suspended (403) means the active tenant is suspended; a
  // tenant switch's own refusal is the switch's error, not that.
  useEffect(() => {
    const onError = (error: unknown, meta: RequestMeta | undefined) => {
      const kind = classifyError(error);
      if (kind === "unauthenticated" && !meta?.handlesUnauthenticated) {
        void forget();
      } else if (kind === "tenant_suspended" && !meta?.switchesTenant) {
        const tenant = queryClient.getQueryData<SessionInfo | null>(SESSION_QUERY_KEY)?.activeTenant?.id;
        if (tenant) setSuspendedTenant(tenant);
      }
    };
    const queries = queryClient.getQueryCache().subscribe((event) => {
      if (event.type === "updated" && event.action.type === "error") onError(event.action.error, event.query.meta);
    });
    const mutations = queryClient.getMutationCache().subscribe((event) => {
      if (event.type === "updated" && event.action.type === "error") {
        onError(event.action.error, event.mutation.options.meta);
      }
    });
    return () => {
      queries();
      mutations();
    };
  }, [queryClient, forget]);

  const reload = useCallback(() => {
    void query.refetch();
  }, [query.refetch]);

  const suspended = suspendedTenant !== null && suspendedTenant === activeTenant;
  const value = useMemo<SessionContextValue>(
    () => ({
      status,
      session,
      error: query.error,
      reload,
      switchTenant,
      suspended,
      endSession,
      forget,
      signedIn,
    }),
    [status, session, query.error, reload, switchTenant, suspended, endSession, forget, signedIn],
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
