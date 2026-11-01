import { type QueryClient, type QueryKey, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { createContext, type ReactNode, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import type { ShownSession } from "../api/client.ts";
import { classifyError } from "../api/errors.ts";
import type { StreamSession } from "../api/sse.ts";
import type { RequestMeta } from "../app/query-client.ts";
import type { SessionInfo, SessionSource, TenantMembership } from "./session.ts";
import type { SessionChannel } from "./session-channel.ts";

export const SESSION_QUERY_KEY = ["session"] as const;
export const PREFERENCES_QUERY_KEY = ["preferences"] as const;

/** Queries about the user, not the tenant: a tenant switch keeps them. */
const USER_QUERIES: ReadonlySet<unknown> = new Set([SESSION_QUERY_KEY[0], PREFERENCES_QUERY_KEY[0]]);

export function isUserQuery(key: QueryKey): boolean {
  return USER_QUERIES.has(key[0]);
}

/**
 * The user and the tenant the pages show, which the API client names on its requests
 * (`createApiClient`'s `shownSession`, Laysi/trustiot-v2#678): the cached session's user and
 * tenant (`null` when it is in none), `undefined` while there is no session to show. The pages
 * render from the same cache, and a change of its tenant or user remounts them (`Layout`,
 * `TenantGate`), so a request names the user and the tenant of the page that sent it.
 */
export function shownSession(queryClient: QueryClient): ShownSession | undefined {
  return shownOf(queryClient.getQueryData<SessionInfo | null>(SESSION_QUERY_KEY)) ?? undefined;
}

/**
 * What the layout tells the user after the session changed under this tab: another tab (or
 * window) switched the tenant, left it, or signed in as someone else.
 */
export type SessionNotice =
  | { kind: "tenant_changed"; tenant: TenantMembership }
  | { kind: "tenant_left" }
  | { kind: "account_changed" };

/** Who the pages show: the user and the tenant; `null` signed out, `undefined` not read yet. */
type Shown = ShownSession;

function shownOf(session: SessionInfo | null | undefined): Shown | null | undefined {
  if (session === undefined || session === null) return session;
  return { user: session.user.id, tenant: session.activeTenant?.id ?? null };
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
  /**
   * Whether a tenant switch is in progress, from any of the places that offer one. They all stay
   * disabled meanwhile, so the user cannot start a second switch while the first is open.
   */
  switching: boolean;
  /** Whether a request of the active tenant was refused because the tenant is suspended. */
  suspended: boolean;
  /** Set when the session changed under this tab, until the user dismisses it. */
  notice: SessionNotice | null;
  dismissNotice(): void;
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
 *
 * Every tab of the browser shares the session (Laysi/trustiot-v2#678). This tab reads it again
 * when another tab says it changed (`channel`), when the tab becomes visible, and when a request
 * comes back `session_changed`; when the user or the tenant it reads differs from the one the pages
 * show, the pages lose what they cached for the previous one and the layout says why.
 */
export function SessionProvider({
  source,
  channel,
  children,
}: {
  source: SessionSource;
  /** Where the browser's tabs announce session changes; none in tests that do not need it. */
  channel?: SessionChannel | undefined;
  children: ReactNode;
}) {
  const queryClient = useQueryClient();
  const query = useQuery({
    queryKey: SESSION_QUERY_KEY,
    queryFn: ({ signal }) => source.loadSession(signal),
    staleTime: 60_000,
    // Another tab may have switched tenants or signed in or out while this one was hidden.
    refetchOnWindowFocus: "always",
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
  const [notice, setNotice] = useState<SessionNotice | null>(null);

  const [pendingSwitches, setPendingSwitches] = useState(0);
  /**
   * Session changes this tab is making itself (a switch, signing in or out): their effects are
   * handled where they are made, and they are no news from another tab.
   */
  const ownChanges = useRef(0);
  /** Who the pages show; the session query's answers are compared with it. */
  const shown = useRef<Shown | null | undefined>(undefined);

  /** Reads the session again, now: another tab may have changed it. */
  const recheck = useCallback(() => {
    void queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY, exact: true });
  }, [queryClient]);

  /** The pages' tenant is gone: its requests stop and every tenant query loses its data. */
  const dropTenantData = useCallback(async () => {
    await queryClient.cancelQueries({ predicate: (cached) => !isUserQuery(cached.queryKey) });
    setSuspendedTenant(null);
    // Reset, not remove: a page that is open keeps observing its query and refetches it for the
    // new tenant. Refetch errors reach the query cache's handler like any other.
    void queryClient.resetQueries({ predicate: (cached) => !isUserQuery(cached.queryKey) });
  }, [queryClient]);

  const forgetCached = useCallback(async () => {
    await queryClient.cancelQueries();
    queryClient.removeQueries({ predicate: (cached) => cached.queryKey[0] !== SESSION_QUERY_KEY[0] });
    setSuspendedTenant(null);
  }, [queryClient]);

  /**
   * The session changed from what the pages showed (`before`) to `next`, and this tab did not ask
   * for it: signed out elsewhere, the user's cached data goes (the layout then sends them to sign
   * in); signed in as someone else, everything cached goes; in another tenant (when `tenants`),
   * the tenant's data goes. The pages remount for the new user or tenant (Layout, TenantGate), so
   * nothing typed into a page of the previous one survives into the new one.
   */
  const changedElsewhere = useCallback(
    (before: Shown | null | undefined, next: SessionInfo | null | undefined, tenants: boolean) => {
      const after = shownOf(next);
      if (!before || after === undefined) return;
      if (after === null) {
        setNotice(null);
        void forgetCached();
      } else if (after.user !== before.user) {
        setNotice({ kind: "account_changed" });
        void forgetCached();
      } else if (tenants && after.tenant !== before.tenant) {
        const tenant = next?.activeTenant;
        setNotice(tenant ? { kind: "tenant_changed", tenant } : { kind: "tenant_left" });
        void dropTenantData();
      }
    },
    [forgetCached, dropTenantData],
  );

  /**
   * Runs a change of the session this tab makes itself. What it changes is handled where it is
   * made: a switch drops the tenant's data, signing in or out (`changesAccount`) everything. A
   * change from another tab that lands meanwhile is compared once it is done: a switch whose
   * session meanwhile became another user's is still handled as that.
   */
  const own = useCallback(
    async <T,>(change: () => Promise<T>, changesAccount: boolean): Promise<T> => {
      const before = shown.current;
      ownChanges.current += 1;
      try {
        return await change();
      } finally {
        ownChanges.current -= 1;
        const next = queryClient.getQueryData<SessionInfo | null>(SESSION_QUERY_KEY);
        shown.current = shownOf(next);
        if (!changesAccount && ownChanges.current === 0) changedElsewhere(before, next, false);
      }
    },
    [queryClient, changedElsewhere],
  );

  // One switch at a time, in the order asked: the scope holds a later switch until the earlier one
  // has finished, onSuccess included, so the server handles them in that order and each answer is
  // cached before the next request leaves. The cached session is then the server's last answer;
  // without the scope, the answer to an earlier switch could arrive last and be cached over the
  // tenant the server ended up in.
  const { mutateAsync: switchOnServer } = useMutation({
    mutationFn: (tenantId: string) => source.switchTenant(tenantId),
    scope: { id: "session.switch_tenant" },
    meta: { switchesTenant: true },
    onSuccess: async (next) => {
      // The preferences are the user's and stay valid; cancelling their first read would leave it
      // pending for good (nothing refetches a query that never goes stale). The session is
      // cancelled too: a read that started before the switch must not replace its answer.
      await queryClient.cancelQueries({ predicate: (cached) => cached.queryKey[0] !== PREFERENCES_QUERY_KEY[0] });
      queryClient.setQueryData(SESSION_QUERY_KEY, next);
      setNotice(null);
      await dropTenantData();
    },
  });

  const switchTenant = useCallback(
    async (tenantId: string) => {
      setPendingSwitches((count) => count + 1);
      try {
        await own(() => switchOnServer(tenantId), false);
        channel?.announce();
      } finally {
        setPendingSwitches((count) => count - 1);
      }
    },
    [own, switchOnServer, channel],
  );

  const endSession = useCallback(
    async (scope: "this" | "everywhere") => {
      await own(() => (scope === "this" ? source.logOut() : source.logOutEverywhere()), true);
      channel?.announce();
    },
    [own, source, channel],
  );

  const forget = useCallback(
    () =>
      own(async () => {
        await forgetCached();
        queryClient.setQueryData(SESSION_QUERY_KEY, null);
        setNotice(null);
      }, true),
    [own, forgetCached, queryClient],
  );

  const signedIn = useCallback(async () => {
    const next = await own(async () => {
      await forgetCached();
      setNotice(null);
      await queryClient.fetchQuery({
        queryKey: SESSION_QUERY_KEY,
        queryFn: ({ signal }) => source.loadSession(signal),
        staleTime: 0,
      });
      // The cached value, which is what `session` becomes: fetchQuery resolves with the answer
      // itself, and the cache keeps the previous object when the answer equals it.
      return queryClient.getQueryData<SessionInfo | null>(SESSION_QUERY_KEY) ?? null;
    }, true);
    channel?.announce();
    return next;
  }, [own, forgetCached, queryClient, source, channel]);

  // What a refused request says about the session, whichever request the page made. A 401 means
  // the session ended (signed out on another device, expired): the user's cached data goes, and
  // the signed-in layout sends them to sign in. A request that answers its own 401s (the sign-in
  // form, signing out everywhere) says so in its meta. tenant_suspended (403) means the active
  // tenant is suspended; a tenant switch's own refusal is the switch's error, not that.
  // session_changed (409) means the session is no longer the user's or in the tenant the pages
  // show: another tab switched the tenant or signed in as someone else, so the session is read
  // again (a change of this tab's own resets the pages anyway).
  useEffect(() => {
    const onError = (error: unknown, meta: RequestMeta | undefined) => {
      const kind = classifyError(error);
      if (kind === "unauthenticated" && !meta?.handlesUnauthenticated) {
        void forget();
      } else if (kind === "tenant_suspended" && !meta?.switchesTenant) {
        const tenant = queryClient.getQueryData<SessionInfo | null>(SESSION_QUERY_KEY)?.activeTenant?.id;
        if (tenant) setSuspendedTenant(tenant);
      } else if (kind === "session_changed" && ownChanges.current === 0) {
        recheck();
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
  }, [queryClient, forget, recheck]);

  // Every answer to the session query, compared with who the pages show. While this tab is making
  // a change of its own, `own` compares once the change is done.
  useEffect(() => {
    shown.current = shownOf(queryClient.getQueryData<SessionInfo | null>(SESSION_QUERY_KEY));
    return queryClient.getQueryCache().subscribe((event) => {
      if (event.type !== "updated" || event.action.type !== "success") return;
      if (event.query.queryKey[0] !== SESSION_QUERY_KEY[0] || ownChanges.current > 0) return;
      const before = shown.current;
      const next = queryClient.getQueryData<SessionInfo | null>(SESSION_QUERY_KEY);
      shown.current = shownOf(next);
      changedElsewhere(before, next, true);
    });
  }, [queryClient, changedElsewhere]);

  // Another tab says the session changed: read it.
  useEffect(() => channel?.subscribe(recheck), [channel, recheck]);

  const reload = useCallback(() => {
    void query.refetch();
  }, [query.refetch]);

  const dismissNotice = useCallback(() => setNotice(null), []);

  const suspended = suspendedTenant !== null && suspendedTenant === activeTenant;
  const switching = pendingSwitches > 0;
  const value = useMemo<SessionContextValue>(
    () => ({
      status,
      session,
      error: query.error,
      reload,
      switchTenant,
      switching,
      suspended,
      notice,
      dismissNotice,
      endSession,
      forget,
      signedIn,
    }),
    [
      status,
      session,
      query.error,
      reload,
      switchTenant,
      switching,
      suspended,
      notice,
      dismissNotice,
      endSession,
      forget,
      signedIn,
    ],
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

/**
 * The session's side of a stream of a tenant's data (`openEventStream`'s `session`): the stream
 * names the user and the tenant the pages show on every connection, and a refusal because the
 * session became another user's or moved to another tenant reads the session again, as a refused
 * request does (Laysi/trustiot-v2#678).
 */
export function useStreamSession(): StreamSession {
  const queryClient = useQueryClient();
  const { reload } = useSession();
  return useMemo(() => ({ shown: () => shownSession(queryClient), changed: reload }), [queryClient, reload]);
}
