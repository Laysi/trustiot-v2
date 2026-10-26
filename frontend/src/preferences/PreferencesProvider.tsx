import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { createContext, type ReactNode, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";

import type { PreferencesApi } from "../api/identity.ts";
import type { Locale } from "../i18n/messages.ts";
import { PREFERENCES_QUERY_KEY, useSession } from "../session/SessionProvider.tsx";
import {
  applyToDocument,
  browserLocale,
  browserTimeZone,
  DARK_QUERY,
  PREFERENCE_FIELDS,
  type PreferenceField,
  type Preferences,
  type PreferencesStore,
  resolveLocale,
  resolveTheme,
  resolveTimeZone,
} from "./preferences.ts";

/** Whose preferences are shown: the browser's copy, or the server's. */
export type PreferencesSource =
  /** Signed out: the browser's copy (the last user's, or the defaults). */
  | "browser"
  /** Signed in, and the server has not answered yet: the browser's copy until it does. */
  | "loading"
  /** Signed in: the server's values, with changes still being saved on top. */
  | "server"
  /** Signed in, and the server could not be read: the browser's copy, and changes are not saved. */
  | "error";

export type UpdateResult = { ok: true } | { ok: false; error: unknown };

export interface PreferencesContextValue {
  /** What applies now: the source's values with unsaved changes on top. */
  preferences: Preferences;
  source: PreferencesSource;
  /** Why the server's preferences could not be read, when `source` is "error". */
  error: unknown;
  /** Reads the server's preferences again. */
  reload(): void;
  /**
   * Changes some preferences. They apply at once; signed in, they are saved to the server, whose
   * answer then applies, and a refused change goes back to the saved value. Resolves with the
   * outcome, so the settings page can show a rejected field's error.
   */
  update(change: Partial<Preferences>): Promise<UpdateResult>;
  /** Whether a change is still being saved. */
  saving: boolean;
  /** The interface's language ("auto" resolved). */
  locale: Locale;
  /** What "auto" stands for in this browser. */
  browserLocale: Locale;
  /** The IANA time zone dates are shown in ("auto" resolved). */
  timeZone: string;
  /** The browser's own time zone, which "auto" stands for. */
  browserZone: string;
  /** The theme the page shows ("system" resolved). */
  theme: "light" | "dark";
}

const PreferencesContext = createContext<PreferencesContextValue | null>(null);

function prefersDark(media: ((query: string) => MediaQueryList) | undefined): boolean {
  return media?.(DARK_QUERY).matches ?? false;
}

/** Changes still being saved, each field with the request that carries it. */
type Pending = Partial<{ [K in PreferenceField]: { value: Preferences[K]; request: number } }>;

function withPending(base: Preferences, pending: Pending): Preferences {
  const result = { ...base };
  if (pending.language) result.language = pending.language.value;
  if (pending.aiLanguage) result.aiLanguage = pending.aiLanguage.value;
  if (pending.timeZone) result.timeZone = pending.timeZone.value;
  if (pending.theme) result.theme = pending.theme.value;
  return result;
}

/**
 * The user's preferences, with the identity service's values authoritative: the same in every
 * browser they sign in to. The browser keeps a copy only to start from (before sign-in and before
 * the server answers). Changes are sent one at a time, in order, so each answer reflects every
 * earlier change, and a change still being saved stays shown over an earlier answer.
 */
export function PreferencesProvider(props: {
  store: PreferencesStore;
  api: PreferencesApi;
  children: ReactNode;
  /** For tests: the browser's languages, time zone and color-scheme query. */
  languages?: readonly string[];
  browserZone?: string;
  matchMedia?: (query: string) => MediaQueryList;
}) {
  const { store, api, children } = props;
  const queryClient = useQueryClient();
  const { status } = useSession();
  const signedIn = status === "signed_in";
  const media = props.matchMedia ?? (typeof window !== "undefined" ? window.matchMedia?.bind(window) : undefined);
  const languages = props.languages ?? (typeof navigator !== "undefined" ? navigator.languages : []);
  const browserZone = props.browserZone ?? browserTimeZone();

  const [local, setLocal] = useState(() => store.load());
  const [pending, setPending] = useState<Pending>({});
  const [dark, setDark] = useState(() => prefersDark(media));
  const requests = useRef(0);

  const query = useQuery({
    queryKey: PREFERENCES_QUERY_KEY,
    queryFn: ({ signal }) => api.loadPreferences(signal),
    enabled: signedIn,
    staleTime: Number.POSITIVE_INFINITY,
  });
  const server = signedIn ? query.data : undefined;

  // Every answer from the server becomes the browser's copy: the next visit starts from it, and so
  // does the sign-in page after this user signs out (read back below when the session ends).
  useEffect(() => {
    if (server) store.save(server);
  }, [server, store]);
  const [wasSignedIn, setWasSignedIn] = useState(signedIn);
  if (wasSignedIn !== signedIn) {
    setWasSignedIn(signedIn);
    if (!signedIn) setLocal(store.load());
  }

  const { mutateAsync: save } = useMutation({
    mutationFn: (change: Partial<Preferences>) => api.updatePreferences(change),
    // One at a time, in the order they were made.
    scope: { id: "preferences" },
  });

  const update = useCallback(
    async (change: Partial<Preferences>): Promise<UpdateResult> => {
      if (!signedIn) {
        setLocal((current) => {
          const next = { ...current, ...change };
          store.save(next);
          return next;
        });
        return { ok: true };
      }
      requests.current += 1;
      const request = requests.current;
      const fields = PREFERENCE_FIELDS.filter((field) => change[field] !== undefined);
      setPending((current) => {
        const next: Pending = { ...current };
        for (const field of fields) Object.assign(next, { [field]: { value: change[field], request } });
        return next;
      });
      try {
        queryClient.setQueryData(PREFERENCES_QUERY_KEY, await save(change));
        return { ok: true };
      } catch (error) {
        return { ok: false, error };
      } finally {
        setPending((current) => {
          const next: Pending = { ...current };
          for (const field of fields) {
            if (next[field]?.request === request) delete next[field];
          }
          return next;
        });
      }
    },
    [signedIn, store, save, queryClient],
  );

  useEffect(() => {
    const listener = media?.(DARK_QUERY);
    if (!listener) return;
    const onChange = (event: MediaQueryListEvent) => setDark(event.matches);
    listener.addEventListener("change", onChange);
    return () => listener.removeEventListener("change", onChange);
  }, [media]);

  const preferences = useMemo(
    () => withPending(server ?? local, signedIn ? pending : {}),
    [server, local, signedIn, pending],
  );
  const locale = resolveLocale(preferences.language, languages);
  const theme = resolveTheme(preferences.theme, dark);
  useEffect(() => {
    applyToDocument(document.documentElement, locale, theme);
  }, [locale, theme]);

  const source: PreferencesSource = !signedIn ? "browser" : server ? "server" : query.isError ? "error" : "loading";
  const reload = useCallback(() => {
    void query.refetch();
  }, [query.refetch]);
  const ownLocale = browserLocale(languages);
  const timeZone = resolveTimeZone(preferences.timeZone, browserZone);
  const saving = Object.keys(pending).length > 0;

  const value = useMemo<PreferencesContextValue>(
    () => ({
      preferences,
      source,
      error: query.error,
      reload,
      update,
      saving,
      locale,
      browserLocale: ownLocale,
      timeZone,
      browserZone,
      theme,
    }),
    [preferences, source, query.error, reload, update, saving, locale, ownLocale, timeZone, browserZone, theme],
  );
  return <PreferencesContext.Provider value={value}>{children}</PreferencesContext.Provider>;
}

export function usePreferences(): PreferencesContextValue {
  const value = useContext(PreferencesContext);
  if (value === null) {
    throw new Error("usePreferences needs a PreferencesProvider");
  }
  return value;
}
