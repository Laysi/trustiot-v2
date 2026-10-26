/**
 * Builds the dashboard sandbox (frontend/sandbox/README.md, 「建置結果」) into dist/sandbox/,
 * next to the SPA that Vite built: the `web` image serves it, and the checking service's image
 * takes the same files. Each script is its own bundle (an immediately invoked function, no
 * imports at run time, no source map), so the sandbox document loads nothing but these files.
 *
 * `bun run build` runs it after Vite, which empties dist/ first. Usage, from the repository root:
 * bun run --cwd frontend build
 */

import { copyFileSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { RUNTIMES } from "../sandbox/src/protocol.ts";

const FRONTEND = fileURLToPath(new URL("..", import.meta.url));
const SANDBOX = join(FRONTEND, "sandbox");

/** The files of one runtime version, under dist/sandbox/v<version>/. */
export const VERSION_FILES = ["sandbox.html", "runtime.js", "d3.js", "sandbox.css", "compare.js"] as const;

/** The checker's host page, under dist/sandbox/. */
export const HOST_FILES = ["check.html", "check.js"] as const;

/** Every sandbox file's path under dist/, as the web service and the checking service serve it. */
export function sandboxPaths(): string[] {
  return [
    ...HOST_FILES.map((name) => `sandbox/${name}`),
    ...RUNTIMES.flatMap((version) => VERSION_FILES.map((name) => `sandbox/v${version}/${name}`)),
  ];
}

/** Bundles one entry point into a single classic script. */
async function bundle(entry: string): Promise<string> {
  const result = await Bun.build({
    entrypoints: [join(SANDBOX, "src", "entries", entry)],
    format: "iife",
    target: "browser",
    minify: true,
    sourcemap: "none",
    define: { "process.env.NODE_ENV": '"production"' },
  });
  if (!result.success || result.outputs.length !== 1) {
    throw new AggregateError(result.logs, `bundling ${entry} failed`);
  }
  const [output] = result.outputs;
  if (output === undefined) throw new Error(`bundling ${entry} wrote nothing`);
  return output.text();
}

/** Writes the sandbox into `dist` (the frontend's dist/). */
export async function buildSandbox(dist: string): Promise<void> {
  const out = join(dist, "sandbox");
  rmSync(out, { recursive: true, force: true });
  const write = (path: string, text: string) => {
    mkdirSync(dirname(join(dist, path)), { recursive: true });
    writeFileSync(join(dist, path), text);
  };
  const [runtime, compare, d3, check] = await Promise.all(
    ["runtime.ts", "compare.ts", "d3.ts", "check.ts"].map((entry) => bundle(entry)),
  );
  // Today one version is built from the current source. When v2 comes, v1's files are frozen:
  // they are kept as built and never rebuilt from changed source (the README's version rule).
  for (const version of RUNTIMES) {
    const directory = `sandbox/v${version}`;
    write(`${directory}/runtime.js`, runtime ?? "");
    write(`${directory}/compare.js`, compare ?? "");
    write(`${directory}/d3.js`, d3 ?? "");
    for (const name of ["sandbox.html", "sandbox.css"]) {
      mkdirSync(join(dist, directory), { recursive: true });
      copyFileSync(join(SANDBOX, "static", name), join(dist, directory, name));
    }
  }
  write("sandbox/check.js", check ?? "");
  copyFileSync(join(SANDBOX, "static", "check.html"), join(out, "check.html"));
}

if (import.meta.main) {
  await buildSandbox(join(FRONTEND, "dist"));
}
