import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { compile } from "tailwindcss";

import { cn } from "../../lib/utils.ts";
import { focusRing } from "./button.tsx";

// The test DOM computes no styles, so the cascade is checked on the CSS the project's Tailwind
// compiles from the classes: Tailwind 4's `outline-none` sets `--tw-outline-style: none` on the
// element, and `focus-visible:outline-2` takes its `outline-style` from that variable, so the
// outline stayed invisible on keyboard focus (Laysi/trustiot-v2#1207).

interface Rule {
  selector: string;
  declarations: Map<string, string>;
}

/** Flattens the compiled CSS (nested rules included) into plain rules; at-rules other than `@layer` are skipped. */
function parseRules(css: string): Rule[] {
  const rules: Rule[] = [];
  let pos = 0;

  function parseBlock(parent: string | null, collect: boolean): Map<string, string> {
    const declarations = new Map<string, string>();
    let buffer = "";
    while (pos < css.length) {
      const char = css[pos++];
      if (char === "{") {
        const prelude = buffer.trim();
        buffer = "";
        if (prelude.startsWith("@")) {
          parseBlock(parent, collect && prelude.startsWith("@layer"));
          continue;
        }
        const selector = parent === null ? prelude : prelude.replaceAll("&", parent);
        const nested = parseBlock(selector, collect);
        if (collect) rules.push({ selector, declarations: nested });
      } else if (char === "}") {
        return declarations;
      } else if (char === ";") {
        const [name, ...value] = buffer.split(":");
        if (name && value.length > 0) declarations.set(name.trim(), value.join(":").trim());
        buffer = "";
      } else {
        buffer += char;
      }
    }
    return declarations;
  }

  parseBlock(null, true);
  return rules;
}

function escapeClass(name: string): string {
  return name.replace(/[:[\]/.]/g, (char) => `\\${char}`);
}

/**
 * The `outline-style` an element with exactly these classes has once it matches `:focus-visible`:
 * the rules that apply win by specificity (a pseudo-class adds one), then by source order, and a
 * `var(--tw-outline-style)` takes the winning value of that custom property, or the registered
 * initial value (`solid`) when no rule sets it.
 */
async function focusVisibleOutlineStyle(classes: string): Promise<string | undefined> {
  const names = classes.split(/\s+/).filter(Boolean);
  const compiler = await compile("@tailwind utilities;");
  const rules = parseRules(compiler.build(names));

  const winner = (property: string): string | undefined => {
    let best: { value: string; specificity: number; order: number } | undefined;
    rules.forEach((rule, order) => {
      const value = rule.declarations.get(property);
      if (value === undefined) return;
      const matched = names.some((name) => {
        const base = `.${escapeClass(name)}`;
        return rule.selector === base || rule.selector === `${base}:focus-visible`;
      });
      if (!matched) return;
      const specificity = rule.selector.endsWith(":focus-visible") ? 2 : 1;
      if (!best || specificity > best.specificity || (specificity === best.specificity && order > best.order)) {
        best = { value, specificity, order };
      }
    });
    return best?.value;
  };

  const style = winner("outline-style");
  if (style === "var(--tw-outline-style)") return winner("--tw-outline-style") ?? "solid";
  return style;
}

describe("focusRing", () => {
  test("draws a solid outline when the control has keyboard focus", async () => {
    expect(await focusVisibleOutlineStyle(focusRing)).toBe("solid");
  });

  test("keeps no outline on an element that is not keyboard-focused", async () => {
    // A mouse click on a button does not match :focus-visible, so the base `outline-none` stays.
    const compiler = await compile("@tailwind utilities;");
    const css = compiler.build(focusRing.split(/\s+/));
    expect(css).toContain("outline-style: none");
  });

  test("is not cancelled by tailwind-merge", () => {
    const merged = cn("inline-flex rounded-md", focusRing, "px-2").split(/\s+/);
    for (const name of focusRing.split(/\s+/)) expect(merged).toContain(name);
  });

  test("the check fails for the combination that hid the ring", async () => {
    const before = "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring";
    expect(await focusVisibleOutlineStyle(before)).toBe("none");
  });
});

/** Every non-test source file under `frontend/src`. */
function sourceFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((entry) => {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) return sourceFiles(path);
    return /\.(ts|tsx)$/.test(entry) && !/\.test\.tsx?$/.test(entry) ? [path] : [];
  });
}

describe("focus outline classes in the source", () => {
  test("no class string cancels the outline style of a keyboard-focus outline without restoring it", () => {
    const root = join(import.meta.dir, "..", "..");
    const offenders: string[] = [];
    for (const file of sourceFiles(root)) {
      const text = readFileSync(file, "utf8");
      for (const literal of text.match(/"[^"\n]*"/g) ?? []) {
        const cancels = /(^|[\s"])outline-(none|hidden)(?=[\s"])/.test(literal);
        const drawsFocus = /focus-visible:outline-\d/.test(literal);
        const restores = /focus-visible:outline-(solid|dashed|dotted|double)/.test(literal);
        if (cancels && drawsFocus && !restores) offenders.push(`${file}: ${literal}`);
      }
    }
    expect(offenders).toEqual([]);
  });
});
