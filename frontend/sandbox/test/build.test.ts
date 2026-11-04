// The sandbox build (tools/build-sandbox.ts; README 「建置結果」): exactly the files the contract
// names, each script one self-contained classic script without network APIs or source maps.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";

import { buildSandbox, sandboxPaths } from "../../tools/build-sandbox.ts";

// Bundling four entry points with the minifier takes a few seconds on a loaded machine.
const BUILD_LIMIT_MS = 120_000;

let dist = "";

beforeAll(async () => {
  dist = mkdtempSync(join(tmpdir(), "sandbox-build-"));
  await buildSandbox(dist);
}, BUILD_LIMIT_MS);

afterAll(() => {
  if (dist !== "") rmSync(dist, { recursive: true, force: true });
});

function listing(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? listing(path) : [relative(dist, path).split("\\").join("/")];
  });
}

const read = (path: string) => readFileSync(join(dist, path), "utf8");
const scripts = () => sandboxPaths().filter((path) => path.endsWith(".js"));

describe("the sandbox build", () => {
  test("writes exactly the sandbox files", () => {
    expect(listing(dist).sort()).toEqual([...sandboxPaths()].sort());
  });

  test("each script is one classic script that imports nothing and has no source map", () => {
    for (const path of scripts()) {
      const text = read(path);
      expect(text.length).toBeGreaterThan(0);
      expect(text).not.toMatch(/^\s*(import|export)\b/m);
      expect(text).not.toContain("sourceMappingURL");
      expect(text).not.toMatch(/\bimport\s*\(/);
    }
  });

  test("no script names a network API", () => {
    for (const path of scripts()) {
      const text = read(path);
      for (const name of ["XMLHttpRequest", "WebSocket", "EventSource", "sendBeacon", "importScripts", "fetch("]) {
        expect({ path, name, found: text.includes(name) }).toEqual({ path, name, found: false });
      }
    }
  });

  test("the entries define what the README says", () => {
    expect(read("sandbox/v1/compare.js")).toContain("tqCompare");
    expect(read("sandbox/check.js")).toContain("tqCheckHost");
    expect(read("sandbox/check.js")).toContain("tqCheckReport");
    expect(read("sandbox/v1/sandbox.html")).toContain('<div id="tq-root"></div>');
  });
});

/** The declarations of the motion layer (static/sandbox.css; README, 「顯示時比對的強制」). */
const NO_MOTION = [
  "transition-property: none !important",
  "animation-duration: 0s !important",
  "animation-delay: 0s !important",
  "animation-play-state: running !important",
  "animation-timeline: auto !important",
];

/**
 * Elements, and each pseudo-element that is a box of its own. Not `::first-letter`, `::first-line`
 * or `::selection`: Chromium animates none of them, and a rule that matches them changes the page.
 */
const MOTION_SELECTORS = [
  "*",
  "::before",
  "::after",
  "::marker",
  "::before::marker",
  "::after::marker",
  "::backdrop",
  "::placeholder",
  "::file-selector-button",
  "::details-content",
  "::scroll-marker",
  "::scroll-marker-group",
  "::scroll-button(*)",
  "::column",
  "::column::scroll-marker",
  "::picker(select)",
  "::picker-icon",
  "::checkmark",
];

/** The text of the block that starts at `open` (a `{`), without its braces. */
function block(text: string, open: number): string {
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === "{") depth++;
    if (text[i] === "}" && --depth === 0) return text.slice(open + 1, i);
  }
  throw new Error("an unclosed block");
}

describe("no motion (static/sandbox.css)", () => {
  const css = () => read("sandbox/v1/sandbox.css").replace(/\/\*[\s\S]*?\*\//g, "");

  test("the sandbox document loads the sheet before anything else, and nothing the page controls before it", () => {
    const head = /<head>([\s\S]*?)<\/head>/.exec(read("sandbox/v1/sandbox.html"))?.[1] ?? "";
    const loads = [...head.matchAll(/<(link|style|script)\b[^>]*>/g)].map((match) => match[0]);
    expect(loads[0]).toBe('<link rel="stylesheet" href="sandbox.css" />');
    expect(loads.filter((tag) => tag.startsWith("<link") || tag.startsWith("<style"))).toHaveLength(1);
    // The page's markup goes into #tq-root, after the head (runtime.ts, `loadPage`).
    expect(read("sandbox/v1/sandbox.html")).toMatch(/<\/head>\s*<body>\s*<div id="tq-root"><\/div>\s*<\/body>/);
  });

  test("its first statement is the motion layer: no name, so a page cannot add to it", () => {
    const text = css();
    expect(text.trimStart().startsWith("@layer {")).toBe(true);
    // The only layer in the sheet.
    expect(text.match(/@layer\b/g)).toHaveLength(1);
  });

  test("the layer has one rule per selector, each with every declaration important", () => {
    const text = css();
    const layer = block(text, text.indexOf("{"));
    const rules = [...layer.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((match) => ({
      selector: (match[1] ?? "").trim(),
      declarations: (match[2] ?? "")
        .split(";")
        .map((declaration) => declaration.trim())
        .filter((declaration) => declaration !== ""),
    }));
    expect(rules.map((rule) => rule.selector)).toEqual(MOTION_SELECTORS);
    for (const rule of rules) {
      expect({ selector: rule.selector, declarations: rule.declarations }).toEqual({
        selector: rule.selector,
        declarations: NO_MOTION,
      });
      // A selector list would be dropped whole by a browser that does not know one of its members.
      expect(rule.selector).not.toContain(",");
    }
  });
});
