import { describe, expect, test } from "bun:test";

import { formatDateTime } from "../i18n/format.ts";
import {
  browserLocale,
  browserPreferencesStore,
  DEFAULT_PREFERENCES,
  fromApi,
  type Preferences,
  parsePreferences,
  resolveLocale,
  resolveTheme,
  resolveTimeZone,
  STORAGE_KEY,
  serializePreferences,
  toApiChange,
} from "./preferences.ts";

class MemoryStorage {
  readonly values = new Map<string, string>();
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

const SAVED: Preferences = { language: "zh-TW", aiLanguage: "en", timeZone: "Asia/Taipei", theme: "dark" };

describe("preferences", () => {
  test("the API's body maps to the SPA's fields and back, value for value", () => {
    expect(fromApi({ language: "zh-TW", ai_language: "en", time_zone: "Asia/Taipei", theme: "dark" })).toEqual(SAVED);
    expect(fromApi({ language: "auto", ai_language: "auto", time_zone: "auto", theme: "system" })).toEqual(
      DEFAULT_PREFERENCES,
    );
    expect(toApiChange(SAVED)).toEqual({
      language: "zh-TW",
      ai_language: "en",
      time_zone: "Asia/Taipei",
      theme: "dark",
    });
  });

  test("a change sends only the fields it changes, so the others keep their saved value", () => {
    expect(toApiChange({ aiLanguage: "auto" })).toStrictEqual({ ai_language: "auto" });
    expect(toApiChange({ timeZone: "auto", theme: "light" })).toStrictEqual({ time_zone: "auto", theme: "light" });
    expect(toApiChange({})).toStrictEqual({});
  });

  test("auto follows the browser's languages; a chosen language does not", () => {
    expect(browserLocale(["zh-TW", "en"])).toBe("zh-TW");
    expect(browserLocale(["zh-Hant-TW"])).toBe("zh-TW");
    expect(browserLocale(["ja-JP", "en-US", "zh-TW"])).toBe("en");
    expect(browserLocale(["ja-JP"])).toBe("en");
    expect(browserLocale([])).toBe("en");
    expect(resolveLocale("auto", ["zh-TW"])).toBe("zh-TW");
    expect(resolveLocale("en", ["zh-TW"])).toBe("en");
    expect(resolveLocale("zh-TW", ["en-US"])).toBe("zh-TW");
  });

  test("auto time zone and system theme resolve; an unknown saved zone falls back to the browser's", () => {
    expect(resolveTimeZone("auto", "Europe/Berlin")).toBe("Europe/Berlin");
    expect(resolveTimeZone("Asia/Taipei", "Europe/Berlin")).toBe("Asia/Taipei");
    expect(resolveTimeZone("Mars/Olympus", "Europe/Berlin")).toBe("Europe/Berlin");
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("system", false)).toBe("light");
    expect(resolveTheme("light", true)).toBe("light");
  });

  test("stored preferences round-trip", () => {
    expect(parsePreferences(serializePreferences(SAVED))).toEqual(SAVED);
    expect(parsePreferences(serializePreferences(DEFAULT_PREFERENCES))).toEqual(DEFAULT_PREFERENCES);
  });

  test("a bad field resets only itself, and a missing or mistyped one counts as bad", () => {
    const stored = (fields: Record<string, unknown>) => JSON.stringify({ v: 2, ...fields });
    expect(parsePreferences(stored({ ...SAVED, timeZone: "Mars/Olympus" }))).toEqual({ ...SAVED, timeZone: "auto" });
    expect(parsePreferences(stored({ ...SAVED, theme: "neon" }))).toEqual({ ...SAVED, theme: "system" });
    expect(parsePreferences(stored({ ...SAVED, language: "fr", aiLanguage: 7 }))).toEqual({
      ...SAVED,
      language: "auto",
      aiLanguage: "auto",
    });
    expect(parsePreferences(stored({ theme: "dark" }))).toEqual({ ...DEFAULT_PREFERENCES, theme: "dark" });
  });

  test("another version, or something that is not an object, gives the defaults", () => {
    // Version 1 kept a resolved locale and no AI language; it is not read, the server's values replace it.
    expect(parsePreferences(JSON.stringify({ v: 1, locale: "en", timeZone: "UTC", theme: "dark" }))).toEqual(
      DEFAULT_PREFERENCES,
    );
    for (const v of [0, 1, 3, "2", null, undefined, true]) {
      expect(parsePreferences(JSON.stringify({ ...SAVED, v }))).toEqual(DEFAULT_PREFERENCES);
    }
    for (const text of [null, "{not json", "null", "7", '"x"', "[]", "[1]", "true"]) {
      expect(parsePreferences(text)).toEqual(DEFAULT_PREFERENCES);
    }
  });

  test("stored members that are not preferences are not kept", () => {
    expect(parsePreferences(JSON.stringify({ v: 2, ...SAVED, extra: "x" }))).toStrictEqual(SAVED);
  });

  test("the browser store saves and loads, and survives storage that throws", () => {
    const storage = new MemoryStorage();
    const store = browserPreferencesStore(storage as unknown as Storage);
    store.save(SAVED);
    expect(JSON.parse(storage.values.get(STORAGE_KEY) ?? "")).toEqual({ v: 2, ...SAVED });
    expect(store.load()).toEqual(SAVED);

    const broken = {
      getItem: () => {
        throw new Error("SecurityError");
      },
      setItem: () => {
        throw new Error("QuotaExceededError");
      },
    } as unknown as Storage;
    const fallback = browserPreferencesStore(broken);
    expect(fallback.load()).toEqual(DEFAULT_PREFERENCES);
    expect(() => fallback.save(SAVED)).not.toThrow();
    expect(browserPreferencesStore(undefined).load()).toEqual(DEFAULT_PREFERENCES);
  });

  test("one instant reads differently in two time zones", () => {
    const instant = "2026-10-20T17:30:00Z";
    const utc = formatDateTime(instant, "en", "UTC");
    const taipei = formatDateTime(instant, "en", "Asia/Taipei");
    expect(utc).toContain("Oct 20, 2026");
    expect(utc).toContain("5:30");
    expect(taipei).toContain("Oct 21, 2026");
    expect(taipei).toContain("1:30");
  });
});
