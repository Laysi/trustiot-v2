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
