// Rules on the sandbox's own source (README 「頁面」 and 「比對程式」): the code that runs next to
// model-written pages parses no HTML strings into the live document, evaluates no strings except
// the page scripts it loads on purpose, opens no network channel, and imports only the pinned d3
// modules besides itself.
import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const SOURCE = fileURLToPath(new URL("../src/", import.meta.url));

function sources(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return sources(path);
    return entry.name.endsWith(".ts") && !entry.name.endsWith(".test.ts") ? [path] : [];
  });
}

/** The code without comments, so a comment may name what the code must not use. */
function code(path: string): string {
  return readFileSync(path, "utf8")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/(^|[^:"'`])\/\/.*$/gm, "$1");
}

const files = sources(SOURCE).map((path) => ({ name: relative(SOURCE, path), text: code(path) }));

const FORBIDDEN: [string, RegExp][] = [
  ["innerHTML", /\binnerHTML\b/],
  ["outerHTML", /\bouterHTML\b/],
  ["insertAdjacentHTML", /\binsertAdjacentHTML\b/],
  ["document.write", /\bwrite(ln)?\s*\(/],
  ["eval", /\beval\b/],
  ["Function constructor", /\bnew\s+Function\b|\bFunction\s*\(/],
  ["fetch", /\bfetch\b/],
  ["XMLHttpRequest", /\bXMLHttpRequest\b/],
  ["WebSocket", /\bWebSocket\b/],
  ["EventSource", /\bEventSource\b/],
  ["sendBeacon", /\bsendBeacon\b/],
  ["importScripts", /\bimportScripts\b/],
  ["Worker", /\bWorker\b/],
  ["RTCPeerConnection", /\bRTCPeerConnection\b/],
  ["window.open", /\.open\s*\(/],
  ["location", /\blocation\b/],
];

const D3_MODULES = [
  "d3-array",
  "d3-axis",
  "d3-format",
  "d3-scale",
  "d3-selection",
  "d3-shape",
  "d3-time",
  "d3-time-format",
];

describe("the sandbox source", () => {
  test("finds its files", () => {
    expect(files.map((file) => file.name)).toContain("runtime.ts");
    expect(files.map((file) => file.name)).toContain("compare.ts");
  });

  test.each(FORBIDDEN)("uses no %s", (_, pattern) => {
    for (const file of files) {
      expect({ file: file.name, match: pattern.exec(file.text)?.[0] ?? null }).toEqual({
        file: file.name,
        match: null,
      });
    }
  });

  test("the page scripts are the only strings turned into code, in one place", () => {
    const uses = files.flatMap((file) =>
      [...file.text.matchAll(/\bnew\s+FunctionConstructor\s*\(/g)].map(() => file.name),
    );
    expect(uses).toEqual(["runtime.ts"]);
    const captures = files.flatMap((file) => [...file.text.matchAll(/\.Function\b/g)].map(() => file.name));
    expect(captures).toEqual(["runtime.ts"]);
  });

  test("imports only the pinned d3 modules, zod through the app's module, and its own files", () => {
    for (const file of files) {
      for (const match of file.text.matchAll(/\bfrom\s+"([^"]+)"/g)) {
        const target = match[1] ?? "";
        const allowed =
          D3_MODULES.includes(target) || target.startsWith("./") || target.startsWith("../") || target === "zod";
        expect({ file: file.name, target, allowed }).toEqual({ file: file.name, target, allowed: true });
        if (target.startsWith("../") && !target.startsWith("../../src/lib/zod")) {
          // Up from entries/ only.
          expect(file.name.startsWith("entries/")).toBe(true);
        }
      }
    }
    const pinned = JSON.parse(readFileSync(new URL("../../package.json", import.meta.url), "utf8")) as {
      dependencies: Record<string, string>;
    };
    for (const module of D3_MODULES) expect(pinned.dependencies[module]).toMatch(/^\d+\.\d+\.\d+$/);
  });
});
