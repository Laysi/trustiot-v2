// The design system (docs/design/system/tokens.css) is the one place token values are written
// down; frontend/src/styles/global.css must agree wherever it defines the same token
// (docs/design/README.md, 「設計系統」). This compares every shared token in both themes.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const FRONTEND_CSS = fileURLToPath(new URL("./global.css", import.meta.url));
const DESIGN_CSS = fileURLToPath(new URL("../../../docs/design/system/tokens.css", import.meta.url));

/**
 * Tokens the frontend names differently, frontend name to design system name (both without the
 * leading dashes). A token that gets a different name in the frontend must be added here in the
 * same pull request, or it is silently left out of the comparison.
 */
const FRONTEND_TO_DESIGN: Readonly<Record<string, string>> = {
  "chart-1": "chart-series-1",
  "chart-2": "chart-series-2",
  "chart-3": "chart-series-3",
  "chart-4": "chart-series-4",
  "chart-5": "chart-series-5",
  "chart-6": "chart-series-6",
};

type Tokens = Map<string, string>;
type Themes = { light: Tokens; dark: Tokens };

/** The custom properties declared directly in the rules whose selector list contains `selector`. */
function declarations(css: string, selector: string): Tokens {
  const tokens: Tokens = new Map();
  const bare = css.replace(/\/\*[\s\S]*?\*\//g, "");
  // Only rules without nested braces: an at-rule such as @media wraps rules, and its inner rules match.
  for (const rule of bare.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    // Statements before the rule (@import ...;) end at the last semicolon.
    const prelude = rule[1] ?? "";
    const selectors = prelude
      .slice(prelude.lastIndexOf(";") + 1)
      .split(",")
      .map((s) => s.trim().replace(/\s+/g, " "));
    if (!selectors.includes(selector)) continue;
    for (const declaration of (rule[2] ?? "").split(";")) {
      const match = /^\s*--([\w-]+)\s*:\s*([\s\S]+?)\s*$/.exec(declaration);
      if (match?.[1] !== undefined && match[2] !== undefined) tokens.set(match[1], match[2].replace(/\s+/g, " "));
    }
  }
  return tokens;
}

/** Expands `var(--x)` aliases recursively against `scope`; an undefined or circular alias throws. */
function resolve(name: string, scope: Tokens, seen: readonly string[] = []): string {
  if (seen.includes(name)) throw new Error(`circular var() chain: ${[...seen, name].join(" -> ")}`);
  const value = scope.get(name);
  if (value === undefined) throw new Error(`--${name} is not defined (chain: ${[...seen, name].join(" -> ")})`);
  return value.replace(/var\(\s*--([\w-]+)\s*\)/g, (_, inner: string) => resolve(inner, scope, [...seen, name]));
}

/** Every token of a theme, resolved in that theme's own scope (dark inherits what it does not redefine). */
function theme(scope: Tokens): Tokens {
  return new Map([...scope.keys()].map((name) => [name, resolve(name, scope)]));
}

function load(file: string, lightSelectors: string[], darkSelector: string): Themes {
  const css = readFileSync(file, "utf8");
  const light: Tokens = new Map(lightSelectors.flatMap((selector) => [...declarations(css, selector)]));
  const dark: Tokens = new Map([...light, ...declarations(css, darkSelector)]);
  return { light: theme(light), dark: theme(dark) };
}

/** The differences for the tokens both files define, one readable line each. */
function mismatches(frontend: Themes, design: Themes): { compared: number; lines: string[] } {
  const lines: string[] = [];
  let compared = 0;
  for (const mode of ["light", "dark"] as const) {
    for (const [name, value] of frontend[mode]) {
      const designName = FRONTEND_TO_DESIGN[name] ?? name;
      const expected = design[mode].get(designName);
      if (expected === undefined) continue;
      compared++;
      if (expected !== value) {
        lines.push(`--${name} (${mode}): global.css has "${value}", tokens.css --${designName} has "${expected}"`);
      }
    }
  }
  return { compared, lines };
}

describe("design system tokens", () => {
  // tokens.css puts the values both themes share (--radius) in a plain :root block beside the light one.
  const frontend = load(FRONTEND_CSS, [":root"], ':root[data-theme="dark"]');
  const design = load(DESIGN_CSS, [":root", '[data-theme="light"]'], '[data-theme="dark"]');

  test("every token both global.css and tokens.css define has the same value in both themes", () => {
    const { compared, lines } = mismatches(frontend, design);
    // 32 shared tokens (31 colors and --radius) on 2026-11-03, in two themes; a parser that stops
    // finding the blocks must fail here instead of comparing nothing.
    expect(compared).toBeGreaterThanOrEqual(64);
    expect(lines).toEqual([]);
  });

  test("the name map points at tokens that exist on both sides", () => {
    for (const [name, designName] of Object.entries(FRONTEND_TO_DESIGN)) {
      expect(frontend.light.has(name)).toBe(true);
      expect(design.light.has(designName)).toBe(true);
    }
  });

  test("a differing value is reported with the token, the theme and both values", () => {
    const changed: Themes = { light: new Map(frontend.light), dark: new Map(frontend.dark) };
    changed.dark.set("chart-3", "#000000");
    expect(mismatches(changed, design).lines).toEqual([
      `--chart-3 (dark): global.css has "#000000", tokens.css --chart-series-3 has "#40a986"`,
    ]);
  });

  test("var() aliases resolve in the theme's own scope", () => {
    expect(frontend.light.get("primary")).toBe("#086650");
    expect(frontend.dark.get("primary")).toBe("oklch(0.87 0.09 168)");
    expect(design.dark.get("primary")).toBe("oklch(0.87 0.09 168)");
  });

  test("an undefined or circular alias is an error", () => {
    expect(() => resolve("a", new Map([["a", "var(--b)"]]))).toThrow("not defined");
    expect(() =>
      resolve(
        "a",
        new Map([
          ["a", "var(--b)"],
          ["b", "var(--a)"],
        ]),
      ),
    ).toThrow("circular");
  });
});
