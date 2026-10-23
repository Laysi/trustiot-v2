import { describe, expect, test } from "bun:test";

import { formatDateTime } from "../i18n/format.ts";
import {
  browserPreferencesStore,
  defaultLocale,
  parsePreferences,
  resolveTheme,
  resolveTimeZone,
  STORAGE_KEY,
  serializePreferences,
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

describe("preferences", () => {
  test("the language defaults from the browser's languages", () => {
    expect(defaultLocale(["zh-TW", "en"])).toBe("zh-TW");
    expect(defaultLocale(["zh-Hant-TW"])).toBe("zh-TW");
    expect(defaultLocale(["ja-JP", "en-US", "zh-TW"])).toBe("en");
    expect(defaultLocale(["ja-JP"])).toBe("en");
    expect(defaultLocale([])).toBe("en");
  });

  test("stored preferences round-trip", () => {
    const preferences = { locale: "en", timeZone: "Asia/Taipei", theme: "dark" } as const;
    expect(parsePreferences(serializePreferences(preferences), ["zh-TW"])).toEqual(preferences);
  });

  test("each bad field falls back to its own default", () => {
    expect(
      parsePreferences(JSON.stringify({ v: 1, locale: "fr", timeZone: "Mars/Olympus", theme: "dark" }), ["zh-TW"]),
    ).toEqual({ locale: "zh-TW", timeZone: "auto", theme: "dark" });
  });

  test("a bad field resets only itself, and a missing or mistyped one counts as bad", () => {
    const languages = ["zh-TW"];
    expect(
      parsePreferences(JSON.stringify({ v: 1, locale: "en", timeZone: "Mars/Olympus", theme: "dark" }), languages),
    ).toEqual({
      locale: "en",
      timeZone: "auto",
      theme: "dark",
    });
    expect(parsePreferences(JSON.stringify({ v: 1, locale: "en", timeZone: "UTC", theme: "neon" }), languages)).toEqual(
      {
        locale: "en",
        timeZone: "UTC",
        theme: "system",
      },
    );
    expect(parsePreferences(JSON.stringify({ v: 1, locale: 7, timeZone: null, theme: "light" }), languages)).toEqual({
      locale: "zh-TW",
      timeZone: "auto",
      theme: "light",
    });
    expect(parsePreferences(JSON.stringify({ v: 1, theme: "dark" }), languages)).toEqual({
      locale: "zh-TW",
      timeZone: "auto",
      theme: "dark",
    });
  });

  test("a version other than 1 gives the defaults, whatever the fields hold", () => {
    const defaults = { locale: "zh-TW", timeZone: "auto", theme: "system" } as const;
    const fields = { locale: "en", timeZone: "UTC", theme: "dark" };
    for (const v of [0, 2, "1", null, undefined, true]) {
      expect(parsePreferences(JSON.stringify({ v, ...fields }), ["zh-TW"])).toEqual(defaults);
    }
    expect(parsePreferences(JSON.stringify(fields), ["zh-TW"])).toEqual(defaults);
    for (const text of ["null", "7", '"x"', "[]", "[1]", "true"]) {
      expect(parsePreferences(text, ["zh-TW"])).toEqual(defaults);
    }
  });

  test("stored members that are not preferences are not kept", () => {
    const stored = JSON.stringify({ v: 1, locale: "en", timeZone: "UTC", theme: "dark", extra: "x" });
    expect(parsePreferences(stored, ["zh-TW"])).toStrictEqual({ locale: "en", timeZone: "UTC", theme: "dark" });
  });

  test("unreadable or other-version values give the defaults", () => {
    const defaults = { locale: "zh-TW", timeZone: "auto", theme: "system" } as const;
    expect(parsePreferences(null, ["zh-TW"])).toEqual(defaults);
    expect(parsePreferences("{not json", ["zh-TW"])).toEqual(defaults);
    expect(parsePreferences(JSON.stringify({ v: 2, locale: "en" }), ["zh-TW"])).toEqual(defaults);
  });

  test("the browser store saves and loads, and survives storage that throws", () => {
    const storage = new MemoryStorage();
    const store = browserPreferencesStore(storage as unknown as Storage, ["en"]);
    store.save({ locale: "zh-TW", timeZone: "UTC", theme: "light" });
    expect(JSON.parse(storage.values.get(STORAGE_KEY) ?? "")).toEqual({
      v: 1,
      locale: "zh-TW",
      timeZone: "UTC",
      theme: "light",
    });
    expect(store.load()).toEqual({ locale: "zh-TW", timeZone: "UTC", theme: "light" });

    const broken = {
      getItem: () => {
        throw new Error("SecurityError");
      },
      setItem: () => {
        throw new Error("QuotaExceededError");
      },
    } as unknown as Storage;
    const fallback = browserPreferencesStore(broken, ["en"]);
    expect(fallback.load()).toEqual({ locale: "en", timeZone: "auto", theme: "system" });
    expect(() => fallback.save({ locale: "en", timeZone: "auto", theme: "dark" })).not.toThrow();
    expect(browserPreferencesStore(undefined, ["zh"]).load().locale).toBe("zh-TW");
  });

  test("auto time zone and system theme resolve", () => {
    expect(resolveTimeZone("auto", "Europe/Berlin")).toBe("Europe/Berlin");
    expect(resolveTimeZone("Asia/Taipei", "Europe/Berlin")).toBe("Asia/Taipei");
    expect(resolveTheme("system", true)).toBe("dark");
    expect(resolveTheme("system", false)).toBe("light");
    expect(resolveTheme("light", true)).toBe("light");
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
