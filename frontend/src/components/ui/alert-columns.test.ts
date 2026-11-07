import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

/**
 * `Alert` (ui/alert.tsx) is `grid-cols-[0_1fr]` without an icon, and `AlertDescription` defaults to
 * the second column. A description pinned to `col-start-1` therefore lands in the 0-wide first
 * column, and its text wraps one or two characters per line, unless the `Alert` it sits in
 * overrides the columns (Laysi/trustiot-v2#1183: the composer's two alerts did not).
 *
 * This is a textual check of the source, not a render: it takes the nearest `<Alert` opening tag
 * before each `col-start-1` description and looks for a `grid-cols-` class in it whose first track
 * is not 0 (`grid-cols-[0_1fr]` is the broken default). It does not follow an `Alert` whose
 * className is built in a variable or in another component, and it does not compute the track
 * sizes: `grid-cols-[1fr]`, `grid-cols-[1fr_auto]` and `grid-cols-1` pass, a variant-prefixed class
 * (`max-sm:grid-cols-1`) alone does not.
 */
const SRC = join(import.meta.dir, "..", "..");

/** Whether an opening tag has an unprefixed `grid-cols-` class whose first track is not 0. */
function overridesColumns(tag: string): boolean {
  return [...tag.matchAll(/(?<![\w:-])grid-cols-(\[[^\]]*\]|\d+)/g)].some(
    ([, tracks]) => !/^\[?0(?:\.0*)?(?:[a-z%]*)(?:[_\]]|$)/.test(tracks ?? ""),
  );
}

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return sourceFiles(path);
    return entry.name.endsWith(".tsx") && !entry.name.endsWith(".test.tsx") ? [path] : [];
  });
}

/** The opening tag starting at `start`, up to the first `>` outside quotes and braces. */
function openingTag(source: string, start: number): string {
  let braces = 0;
  let quote: string | null = null;
  for (let index = start; index < source.length; index += 1) {
    const char = source[index];
    if (quote) {
      if (char === quote) quote = null;
    } else if (char === '"' || char === "'" || char === "`") {
      quote = char;
    } else if (char === "{") {
      braces += 1;
    } else if (char === "}") {
      braces -= 1;
    } else if (char === ">" && braces === 0) {
      return source.slice(start, index + 1);
    }
  }
  return source.slice(start);
}

describe("alert descriptions in column 1", () => {
  test("every col-start-1 AlertDescription sits in an Alert that overrides the columns", () => {
    const offenders: string[] = [];
    let checked = 0;
    for (const file of sourceFiles(SRC)) {
      const source = readFileSync(file, "utf8");
      for (const match of source.matchAll(/<AlertDescription\b[^>]*\bcol-start-1\b/g)) {
        checked += 1;
        const alerts = [...source.slice(0, match.index).matchAll(/<Alert(?=[\s>])/g)];
        const alert = alerts.at(-1);
        const tag = alert ? openingTag(source, alert.index) : "";
        if (!overridesColumns(tag)) {
          const line = source.slice(0, match.index).split("\n").length;
          offenders.push(`${file.slice(SRC.length + 1)}:${line}`);
        }
      }
    }
    // The scan must find the descriptions it guards, or a changed pattern would pass it vacuously.
    expect(checked).toBeGreaterThan(40);
    expect(offenders).toEqual([]);
  });

  test("the scan flags an Alert without a column override", () => {
    const source = '<Alert variant="destructive">\n<AlertDescription className="col-start-1">x</AlertDescription>';
    const alert = [...source.matchAll(/<Alert(?=[\s>])/g)].at(-1);
    expect(overridesColumns(openingTag(source, alert?.index ?? 0))).toBe(false);
  });

  test("an override counts only when its first track is usable", () => {
    expect(overridesColumns('<Alert className="grid-cols-[1fr]">')).toBe(true);
    expect(overridesColumns('<Alert className="grid-cols-[1fr_auto] items-center">')).toBe(true);
    expect(overridesColumns('<Alert className="grid-cols-1">')).toBe(true);
    expect(overridesColumns('<Alert className="grid-cols-[0_1fr]">')).toBe(false);
    expect(overridesColumns('<Alert className="grid-cols-[0px_1fr]">')).toBe(false);
    expect(overridesColumns('<Alert className="grid-cols-[0fr_1fr]">')).toBe(false);
    expect(overridesColumns('<Alert className="grid-cols-[0.5fr_1fr]">')).toBe(true);
    expect(overridesColumns('<Alert className="max-sm:grid-cols-1">')).toBe(false);
    expect(overridesColumns("<Alert>")).toBe(false);
  });
});
