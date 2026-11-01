import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "vite";

import { RELOAD_KEY } from "./preload-error.ts";

/**
 * `reloadOnPreloadError` (preload-error.ts) answers the `vite:preloadError` event, which Vite's
 * preload helper raises when a chunk cannot be loaded: after a deployment, a tab that is still
 * open asks for a chunk the new version no longer has. This builds the real application and
 * checks, in the output, that every code-split chunk is loaded through that helper, so a page
 * added to the router with `lazy` is covered without the route having to do anything.
 */

const CONFIG = fileURLToPath(new URL("../../vite.config.ts", import.meta.url));
/** A real build takes a few seconds, more on a loaded machine; nothing here depends on how long. */
const BUILD_TIMEOUT_MS = 180_000;

const files = new Map<string, string>();
let outDir = "";

beforeAll(async () => {
  outDir = mkdtempSync(join(tmpdir(), "trustiot-chunks-"));
  // Not minified, so the helper keeps the name Vite gives it and the calls can be matched by name.
  await build({ configFile: CONFIG, logLevel: "silent", build: { outDir, emptyOutDir: true, minify: false } });
  const directory = join(outDir, "static");
  for (const name of readdirSync(directory)) {
    if (name.endsWith(".js")) files.set(name, readFileSync(join(directory, name), "utf8"));
  }
}, BUILD_TIMEOUT_MS);

afterAll(() => {
  if (outDir !== "") rmSync(outDir, { recursive: true, force: true });
});

/** The chunk files a source imports with `import("./chunk.js")`. */
function chunkImports(source: string): string[] {
  return [...source.matchAll(/\bimport\(\s*(["'`])(\.\/[^"'`]+\.js)\1\s*\)/g)].map((match) => match[2] as string);
}

describe("the chunks of the build", () => {
  test("the router's pages are split out of the entry bundle", () => {
    const imported = new Set([...files.values()].flatMap(chunkImports));
    expect(imported.size).toBeGreaterThan(0);
    for (const chunk of imported) {
      expect(files.has(chunk.slice(2))).toBe(true);
    }
  });

  test("every dynamic import of a chunk goes through the preload helper", () => {
    for (const [name, source] of files) {
      const imports = chunkImports(source).length;
      const wrapped = [...source.matchAll(/__vitePreload\(\s*\(\)\s*=>\s*import\(\s*(["'`])\.\/[^"'`]+\.js\1\s*\)/g)]
        .length;
      expect({ name, unwrapped: imports - wrapped }).toEqual({ name, unwrapped: 0 });
    }
  });

  test("the preload helper raises vite:preloadError and the entry reloads on it", () => {
    const definitions = [...files].filter(([, source]) =>
      /\b(?:function|const|var|let)\s+__vitePreload\b/.test(source),
    );
    expect(definitions).toHaveLength(1);
    const [definingName, defining] = definitions[0] as [string, string];
    // The event is created, given the failure and dispatched on the window; unless it is
    // prevented, the failure goes on to the router's error boundary.
    expect(defining).toContain('new Event("vite:preloadError"');
    expect(defining).toContain("window.dispatchEvent(");
    expect(defining).toContain("defaultPrevented");
    for (const [name, source] of files) {
      if (name !== definingName && source.includes("__vitePreload(")) {
        expect(source).toMatch(
          new RegExp(`import\\s*\\{[^}]*__vitePreload[^}]*\\}\\s*from\\s*["']\\./${definingName}["']`),
        );
      }
    }
    // The listener that answers the event is part of the same build, with its once-a-minute guard.
    expect([...files.values()].some((source) => source.includes(RELOAD_KEY))).toBe(true);
  });
});
