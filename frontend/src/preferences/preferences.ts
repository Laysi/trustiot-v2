import { isLocale, type Locale } from "../i18n/messages.ts";

export type Theme = "system" | "light" | "dark";
export const THEMES: readonly Theme[] = ["system", "light", "dark"];

/** "auto" follows the browser's time zone; anything else is an IANA name such as Asia/Taipei. */
export type TimeZonePreference = "auto" | (string & {});

export interface Preferences {
  locale: Locale;
  timeZone: TimeZonePreference;
  theme: Theme;
}

/**
 * Where preferences are kept. The browser's storage for now; a server-side store (per user, so
 * they follow the user to another device) can implement the same interface later.
 */
export interface PreferencesStore {
  load(): Preferences;
  save(preferences: Preferences): void;
}

/** The storage key and the format version of the stored value. */
export const STORAGE_KEY = "trustiot.preferences";
const VERSION = 1;

/** The first browser language we have a translation for decides; English otherwise. */
export function defaultLocale(languages: readonly string[]): Locale {
  for (const language of languages) {
    const tag = language.toLowerCase();
    if (tag === "zh" || tag.startsWith("zh-")) return "zh-TW";
    if (tag === "en" || tag.startsWith("en-")) return "en";
  }
  return "en";
}

export function isTimeZone(zone: string): boolean {
  try {
    new Intl.DateTimeFormat(undefined, { timeZone: zone });
    return true;
  } catch {
    return false;
  }
}

function isTheme(value: unknown): value is Theme {
  return typeof value === "string" && (THEMES as readonly string[]).includes(value);
}

export function defaults(languages: readonly string[]): Preferences {
  return { locale: defaultLocale(languages), timeZone: "auto", theme: "system" };
}

/**
 * Preferences from their stored text. Each field is checked on its own and falls back to its
 * default, so one bad value (an old time zone name, a hand-edited entry) does not reset the rest.
 */
export function parsePreferences(raw: string | null, languages: readonly string[]): Preferences {
  const fallback = defaults(languages);
  if (raw === null) {
    return fallback;
  }
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return fallback;
  }
  if (typeof value !== "object" || value === null || (value as { v?: unknown }).v !== VERSION) {
    return fallback;
  }
  const stored = value as Record<string, unknown>;
  return {
    locale: isLocale(stored.locale) ? stored.locale : fallback.locale,
    timeZone:
      stored.timeZone === "auto" || (typeof stored.timeZone === "string" && isTimeZone(stored.timeZone))
        ? stored.timeZone
        : fallback.timeZone,
    theme: isTheme(stored.theme) ? stored.theme : fallback.theme,
  };
}

export function serializePreferences(preferences: Preferences): string {
  return JSON.stringify({ v: VERSION, ...preferences });
}

/**
 * A store in the browser's storage. Storage can be missing or throw (private browsing, a full
 * quota); then the preferences last only until the page is reloaded.
 */
export function browserPreferencesStore(storage: Storage | undefined, languages: readonly string[]): PreferencesStore {
  return {
    load() {
      try {
        return parsePreferences(storage?.getItem(STORAGE_KEY) ?? null, languages);
      } catch {
        return defaults(languages);
      }
    },
    save(preferences) {
      try {
        storage?.setItem(STORAGE_KEY, serializePreferences(preferences));
      } catch {
        // Not kept; the current page still uses the new preferences.
      }
    },
  };
}

/** The IANA time zone to format with. */
export function resolveTimeZone(preference: TimeZonePreference, browserZone: string): string {
  return preference === "auto" ? browserZone : preference;
}

export function browserTimeZone(): string {
  return new Intl.DateTimeFormat().resolvedOptions().timeZone;
}

/** The theme the page shows: "system" follows the operating system's setting. */
export function resolveTheme(theme: Theme, prefersDark: boolean): "light" | "dark" {
  if (theme === "system") {
    return prefersDark ? "dark" : "light";
  }
  return theme;
}
