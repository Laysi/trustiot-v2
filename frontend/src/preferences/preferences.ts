import { LOCALES, type Locale } from "../i18n/messages.ts";
import { z } from "../lib/zod.ts";

export const THEMES = ["system", "light", "dark"] as const;
export type Theme = (typeof THEMES)[number];

export function isTimeZone(zone: string): boolean {
  try {
    new Intl.DateTimeFormat(undefined, { timeZone: zone });
    return true;
  } catch {
    return false;
  }
}

/** "auto" follows the browser's time zone; anything else is an IANA name such as Asia/Taipei. */
const TimeZoneSchema = z.union([z.literal("auto"), z.string().refine(isTimeZone)]);
export type TimeZonePreference = z.output<typeof TimeZoneSchema>;

/** The preferences, each field with the values it may have. */
const PreferencesShape = z.object({
  locale: z.enum(LOCALES),
  timeZone: TimeZoneSchema,
  theme: z.enum(THEMES),
});
export type Preferences = z.output<typeof PreferencesShape>;

/** The storage key and the format version of the stored value. */
export const STORAGE_KEY = "trustiot.preferences";
const VERSION = 1;

/**
 * What is stored: the version and every field. A field that is missing or not allowed falls back
 * to its own default, so one bad value (an old time zone name, a hand-edited entry) does not reset
 * the rest; another version, or something that is not an object, fails as a whole.
 */
function storedSchema(fallback: Preferences) {
  return z
    .object({
      v: z.literal(VERSION),
      locale: PreferencesShape.shape.locale.catch(fallback.locale),
      timeZone: PreferencesShape.shape.timeZone.catch(fallback.timeZone),
      theme: PreferencesShape.shape.theme.catch(fallback.theme),
    })
    .transform(({ locale, timeZone, theme }): Preferences => ({ locale, timeZone, theme }));
}

/**
 * Where preferences are kept. The browser's storage for now; a server-side store (per user, so
 * they follow the user to another device) can implement the same interface later.
 */
export interface PreferencesStore {
  load(): Preferences;
  save(preferences: Preferences): void;
}

/** The first browser language we have a translation for decides; English otherwise. */
export function defaultLocale(languages: readonly string[]): Locale {
  for (const language of languages) {
    const tag = language.toLowerCase();
    if (tag === "zh" || tag.startsWith("zh-")) return "zh-TW";
    if (tag === "en" || tag.startsWith("en-")) return "en";
  }
  return "en";
}

export function defaults(languages: readonly string[]): Preferences {
  return { locale: defaultLocale(languages), timeZone: "auto", theme: "system" };
}

/**
 * Preferences from their stored text (browser storage, so anything can be in it). Each field
 * falls back to its own default; unreadable text or another version gives the defaults.
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
  const parsed = storedSchema(fallback).safeParse(value);
  return parsed.success ? parsed.data : fallback;
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
