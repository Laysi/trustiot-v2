import { createContext, type ReactNode, useCallback, useContext, useEffect, useMemo, useState } from "react";

import {
  browserTimeZone,
  type Preferences,
  type PreferencesStore,
  resolveTheme,
  resolveTimeZone,
} from "./preferences.ts";

export interface PreferencesContextValue {
  preferences: Preferences;
  /** Changes some preferences and keeps them in the store. */
  update(change: Partial<Preferences>): void;
  /** The IANA time zone dates are shown in ("auto" resolved). */
  timeZone: string;
  /** The browser's own time zone, which "auto" stands for. */
  browserZone: string;
  /** The theme the page shows ("system" resolved). */
  theme: "light" | "dark";
}

const PreferencesContext = createContext<PreferencesContextValue | null>(null);

const DARK_QUERY = "(prefers-color-scheme: dark)";

function prefersDark(media: ((query: string) => MediaQueryList) | undefined): boolean {
  return media?.(DARK_QUERY).matches ?? false;
}

export function PreferencesProvider(props: {
  store: PreferencesStore;
  children: ReactNode;
  /** For tests: the browser's time zone and color-scheme query. */
  browserZone?: string;
  matchMedia?: (query: string) => MediaQueryList;
}) {
  const { store, children } = props;
  const media = props.matchMedia ?? (typeof window !== "undefined" ? window.matchMedia?.bind(window) : undefined);
  const browserZone = props.browserZone ?? browserTimeZone();
  const [preferences, setPreferences] = useState(() => store.load());
  const [dark, setDark] = useState(() => prefersDark(media));

  useEffect(() => {
    const query = media?.(DARK_QUERY);
    if (!query) return;
    const listener = (event: MediaQueryListEvent) => setDark(event.matches);
    query.addEventListener("change", listener);
    return () => query.removeEventListener("change", listener);
  }, [media]);

  const theme = resolveTheme(preferences.theme, dark);
  useEffect(() => {
    // The stylesheet picks the colors from data-theme (no inline style: the CSP allows none).
    document.documentElement.dataset.theme = theme;
  }, [theme]);
  useEffect(() => {
    document.documentElement.lang = preferences.locale;
  }, [preferences.locale]);

  const update = useCallback(
    (change: Partial<Preferences>) => {
      setPreferences((current) => {
        const next = { ...current, ...change };
        store.save(next);
        return next;
      });
    },
    [store],
  );

  const value = useMemo<PreferencesContextValue>(
    () => ({
      preferences,
      update,
      timeZone: resolveTimeZone(preferences.timeZone, browserZone),
      browserZone,
      theme,
    }),
    [preferences, update, browserZone, theme],
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
