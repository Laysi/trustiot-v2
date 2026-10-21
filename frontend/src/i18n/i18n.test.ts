// A translation missing from one language fails here (and a key used in code but missing from
// en.json fails the type check). CI runs this in the frontend job.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

import en from "./locales/en.json";
import zhTW from "./locales/zh-TW.json";
import { CATALOGS, isMessageId, LOCALES, MESSAGE_KEY_FORMAT } from "./messages.ts";
import { placeholders, translate } from "./translate.ts";

const SRC = fileURLToPath(new URL("..", import.meta.url));

const catalogs: Record<string, Record<string, string>> = { en, "zh-TW": zhTW };

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sources(path);
    return /\.(ts|tsx)$/.test(name) && !/\.test\.tsx?$/.test(name) && !name.endsWith(".gen.ts") ? [path] : [];
  });
}

describe("translations", () => {
  test("every language has exactly the same keys", () => {
    const keys = Object.keys(en).sort();
    for (const [locale, catalog] of Object.entries(catalogs)) {
      const own = Object.keys(catalog).sort();
      expect({
        locale,
        missing: keys.filter((key) => !own.includes(key)),
        extra: own.filter((key) => !keys.includes(key)),
      }).toEqual({
        locale,
        missing: [],
        extra: [],
      });
    }
  });

  test("the catalogs cover every supported locale", () => {
    expect(Object.keys(CATALOGS).sort()).toEqual([...LOCALES].sort());
  });

  test("no message is empty", () => {
    for (const [locale, catalog] of Object.entries(catalogs)) {
      const empty = Object.entries(catalog)
        .filter(([, message]) => message.trim() === "")
        .map(([key]) => key);
      expect({ locale, empty }).toEqual({ locale, empty: [] });
    }
  });

  test("every key has the same placeholders in every language", () => {
    const differ: string[] = [];
    for (const [key, message] of Object.entries(en)) {
      const expected = placeholders(message);
      for (const [locale, catalog] of Object.entries(catalogs)) {
        const actual = placeholders(catalog[key] ?? "");
        if (actual.join() !== expected.join()) differ.push(`${locale} ${key}: {${actual}} instead of {${expected}}`);
      }
    }
    expect(differ).toEqual([]);
  });

  test("every key has the server's message key format", () => {
    expect(Object.keys(en).filter((key) => !MESSAGE_KEY_FORMAT.test(key))).toEqual([]);
  });

  test("every key passed to t() in the source exists", () => {
    const unknown: string[] = [];
    for (const file of sources(SRC)) {
      const text = readFileSync(file, "utf8");
      for (const match of text.matchAll(/\bt\(\s*"([^"]+)"/g)) {
        const key = match[1] as string;
        if (!isMessageId(key)) unknown.push(`${relative(SRC, file)}: ${key}`);
      }
    }
    expect(unknown).toEqual([]);
  });

  test("the scan finds the keys pages use", () => {
    // Guards the regex above: if it stopped matching, the previous test would pass vacuously.
    const found = sources(SRC).flatMap((file) =>
      Array.from(readFileSync(file, "utf8").matchAll(/\bt\(\s*"([^"]+)"/g), (match) => match[1]),
    );
    expect(found).toContain("page.home.title");
    expect(found).toContain("page.not_found.title");
  });
});

describe("translate", () => {
  test("fills in placeholders and formats numbers for the locale", () => {
    expect(translate("en", "tenant.name_invalid", { max: 1000 })).toBe("Enter a name of 1 to 1,000 characters.");
    expect(translate("zh-TW", "tenant.name_invalid", { max: 100 })).toBe("名稱需為 1 到 100 個字元。");
  });

  test("a placeholder without a value stays visible", () => {
    expect(translate("en", "tenant.name_invalid", {})).toBe("Enter a name of 1 to {max} characters.");
  });

  test("values are inserted as text, never interpreted", () => {
    expect(translate("en", "page.not_found.body", { path: "/<b>{max}</b>" })).toBe(
      "The page /<b>{max}</b> doesn't exist or has moved.",
    );
  });
});
