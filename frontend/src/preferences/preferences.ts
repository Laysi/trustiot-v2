import type { components } from "../api/schema.gen.ts";
import { isLocale, type Locale } from "../i18n/messages.ts";
import { z } from "../lib/zod.ts";

/**
 * The user's preferences as the SPA keeps them. They mirror the identity service's
 * (`GET`/`PATCH /api/identity/v1/preferences`, services/identity/README.md 「偏好設定」), which is
 * where they live: the same values in every browser the user signs in to. Each value has the
 * API's meaning, `auto` included, and is resolved only when it is applied (`resolveLocale`,
 * `resolveTimeZone`, `resolveTheme`), so what is shown and what is saved never drift apart.
 */
export const LANGUAGES = ["auto", "zh-TW", "en"] as const;
export type LanguagePreference = (typeof LANGUAGES)[number];

export const THEMES = ["system", "light", "dark"] as const;
export type Theme = (typeof THEMES)[number];

export interface Preferences {
  /** The interface's language; `auto` follows the browser's languages. */
  language: LanguagePreference;
  /** The language AI answers are written in; `auto` answers in the language of the question. */
  aiLanguage: LanguagePreference;
  /** `auto` follows the browser; anything else is an IANA name such as Asia/Taipei. */
  timeZone: string;
  theme: Theme;
}
export type PreferenceField = keyof Preferences;
export const PREFERENCE_FIELDS: readonly PreferenceField[] = ["language", "aiLanguage", "timeZone", "theme"];

/** The identity service's defaults (`auto`, `auto`, `auto`, `system`), also what a new browser starts with. */
export const DEFAULT_PREFERENCES: Preferences = {
  language: "auto",
  aiLanguage: "auto",
  timeZone: "auto",
  theme: "system",
};

type ApiPreferences = components["schemas"]["Preferences"];
type ApiChange = components["schemas"]["UpdatePreferences"];

/** The API's body for each field, by the JSON Pointer its 422 field errors name. */
export const FIELD_POINTERS: Readonly<Record<PreferenceField, string>> = {
  language: "/language",
  aiLanguage: "/ai_language",
  timeZone: "/time_zone",
  theme: "/theme",
};

export function fromApi(body: ApiPreferences): Preferences {
  return { language: body.language, aiLanguage: body.ai_language, timeZone: body.time_zone, theme: body.theme };
}

/** The `PATCH` body for a change: only the fields given, so the others keep their saved value. */
export function toApiChange(change: Partial<Preferences>): ApiChange {
  const body: ApiChange = {};
  if (change.language !== undefined) body.language = change.language;
  if (change.aiLanguage !== undefined) body.ai_language = change.aiLanguage;
  if (change.timeZone !== undefined) body.time_zone = change.timeZone;
  if (change.theme !== undefined) body.theme = change.theme;
  return body;
}

export function isTimeZone(zone: string): boolean {
  try {
    new Intl.DateTimeFormat(undefined, { timeZone: zone });
    return true;
  } catch {
    return false;
  }
}

/**
 * The browser's copy (key and format version). It is only provisional: what the page shows before
 * the user signs in and before the server answers, so the first paint already has their theme
 * and language. After sign-in the server's values replace it, and each answer is copied back here.
 */
export const STORAGE_KEY = "trustiot.preferences";
const VERSION = 2;

/**
 * What is stored: the version and every field. A field that is missing or not allowed falls back
 * to its default, so one bad value (a time zone this browser does not know, a hand-edited entry)
 * does not reset the rest; another version (1 kept a resolved locale and no AI language), or
 * something that is not an object, gives the defaults.
 */
const StoredSchema = z
  .object({
    v: z.literal(VERSION),
    language: z.enum(LANGUAGES).catch(DEFAULT_PREFERENCES.language),
    aiLanguage: z.enum(LANGUAGES).catch(DEFAULT_PREFERENCES.aiLanguage),
    timeZone: z.union([z.literal("auto"), z.string().refine(isTimeZone)]).catch(DEFAULT_PREFERENCES.timeZone),
    theme: z.enum(THEMES).catch(DEFAULT_PREFERENCES.theme),
  })
  .transform(({ language, aiLanguage, timeZone, theme }): Preferences => ({ language, aiLanguage, timeZone, theme }));

/** Where the browser keeps its provisional copy. */
export interface PreferencesStore {
  load(): Preferences;
  save(preferences: Preferences): void;
}

/** Preferences from their stored text (browser storage, so anything can be in it). */
export function parsePreferences(raw: string | null): Preferences {
  if (raw === null) {
    return DEFAULT_PREFERENCES;
  }
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return DEFAULT_PREFERENCES;
  }
  const parsed = StoredSchema.safeParse(value);
  return parsed.success ? parsed.data : DEFAULT_PREFERENCES;
}

export function serializePreferences(preferences: Preferences): string {
  return JSON.stringify({ v: VERSION, ...preferences });
}

/**
 * The copy in the browser's storage. Storage can be missing or throw (private browsing, a full
 * quota); then the page starts from the defaults until the server answers.
 */
export function browserPreferencesStore(storage: Storage | undefined): PreferencesStore {
  return {
    load() {
      try {
        return parsePreferences(storage?.getItem(STORAGE_KEY) ?? null);
      } catch {
        return DEFAULT_PREFERENCES;
      }
    },
    save(preferences) {
      try {
        storage?.setItem(STORAGE_KEY, serializePreferences(preferences));
      } catch {
        // Not kept; the server still has them.
      }
    },
  };
}

/** The first browser language we have a translation for decides; English otherwise. */
export function browserLocale(languages: readonly string[]): Locale {
  for (const language of languages) {
    const tag = language.toLowerCase();
    if (tag === "zh" || tag.startsWith("zh-")) return "zh-TW";
    if (tag === "en" || tag.startsWith("en-")) return "en";
  }
  return "en";
}

/** The language the interface is shown in. */
export function resolveLocale(preference: LanguagePreference, languages: readonly string[]): Locale {
  return isLocale(preference) ? preference : browserLocale(languages);
}

/**
 * The IANA time zone to format with. A saved name this browser does not know (its time zone
 * data is older than the server's) falls back to the browser's own zone instead of failing.
 */
export function resolveTimeZone(preference: string, browserZone: string): string {
  return preference === "auto" || !isTimeZone(preference) ? browserZone : preference;
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

export const DARK_QUERY = "(prefers-color-scheme: dark)";

/**
 * Puts the theme and language on `<html>`: the stylesheet picks its colors from `data-theme` (no
 * inline style: the CSP allows none), and `lang` lets assistive technology read the page right.
 */
export function applyToDocument(root: HTMLElement, locale: Locale, theme: "light" | "dark"): void {
  root.dataset.theme = theme;
  root.lang = locale;
}
