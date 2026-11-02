import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { build, type Plugin } from "vite";

import { FRAMEWORK_MODULE } from "../../vite.config.ts";

/**
 * The `framework` chunk (vite.config.ts, Laysi/trustiot-v2#977) exists so that a browser which
 * cached it keeps using it after a deployment that only changed application code. That holds only
 * while its hash-only file name does not change, so this builds the application twice, the second
 * time with different application code, and compares the files.
 */

const CONFIG = fileURLToPath(new URL("../../vite.config.ts", import.meta.url));
const ENTRY = fileURLToPath(new URL("../main.tsx", import.meta.url));
/** A real build takes a few seconds, more on a loaded machine; nothing here depends on how long. */
const BUILD_TIMEOUT_MS = 180_000;
/** A string that React 19 puts into its element symbol: present in the framework chunk only. */
const REACT_MARKER = "react.transitional.element";

interface Built {
  /** File name to content hash. */
  files: Map<string, string>;
  sources: Map<string, string>;
}

const directories: string[] = [];

/** Builds the application, with `variant` added to the entry module when it is given. */
async function buildApp(variant?: string): Promise<Built> {
  const outDir = mkdtempSync(join(tmpdir(), "trustiot-framework-"));
  directories.push(outDir);
  const plugins: Plugin[] =
    variant === undefined
      ? []
      : [
          {
            name: "application-code-variant",
            transform(code, id) {
              return id === ENTRY ? `${code}\nglobalThis.__variant = ${JSON.stringify(variant)};\n` : null;
            },
          },
        ];
  await build({ configFile: CONFIG, logLevel: "silent", plugins, build: { outDir, emptyOutDir: true } });
  const directory = join(outDir, "static");
  const files = new Map<string, string>();
  const sources = new Map<string, string>();
  for (const name of readdirSync(directory)) {
    if (!name.endsWith(".js")) continue;
    const source = readFileSync(join(directory, name), "utf8");
    files.set(name, createHash("sha256").update(source).digest("hex"));
    sources.set(name, source);
  }
  return { files, sources };
}

let first: Built;
let second: Built;

beforeAll(async () => {
  first = await buildApp();
  second = await buildApp("second deployment");
}, BUILD_TIMEOUT_MS * 2);

afterAll(() => {
  for (const directory of directories) rmSync(directory, { recursive: true, force: true });
});

/** The chunk files that contain React's code. */
function frameworkChunks(built: Built): string[] {
  return [...built.sources].filter(([, source]) => source.includes(REACT_MARKER)).map(([name]) => name);
}

describe("the framework chunk", () => {
  test("react, react-dom and the router live in one chunk of their own", () => {
    const chunks = frameworkChunks(first);
    expect(chunks).toHaveLength(1);
    const chunk = chunks[0] as string;
    // It is loaded through a static import, so index.html's modulepreload covers it.
    const importers = [...first.sources].filter(([name, other]) => name !== chunk && other.includes(chunk));
    expect(importers.length).toBeGreaterThan(0);
  });

  test("the module pattern takes the packages and not look-alike names", () => {
    expect(FRAMEWORK_MODULE.test("/repo/node_modules/react-dom/cjs/react-dom.production.js")).toBe(true);
    expect(FRAMEWORK_MODULE.test("/repo/node_modules/@tanstack/query-core/build/modern/index.js")).toBe(true);
    expect(FRAMEWORK_MODULE.test("C:\\repo\\node_modules\\zod\\v4\\index.js")).toBe(true);
    expect(FRAMEWORK_MODULE.test("/repo/node_modules/react-remove-scroll/dist/index.js")).toBe(false);
    expect(FRAMEWORK_MODULE.test("/repo/node_modules/lucide-react/dist/index.js")).toBe(false);
    expect(FRAMEWORK_MODULE.test("/repo/src/react/index.ts")).toBe(false);
  });

  test("a build that only changes application code keeps the framework chunk's file name", () => {
    const chunk = frameworkChunks(first)[0] as string;
    expect(frameworkChunks(second)).toEqual([chunk]);
    expect(second.files.get(chunk)).toBe(first.files.get(chunk));
    // The change is real: the application's own files did get new names.
    const applicationNames = (built: Built) => [...built.files.keys()].filter((name) => name !== chunk).sort();
    expect(applicationNames(second)).not.toEqual(applicationNames(first));
  });
});
