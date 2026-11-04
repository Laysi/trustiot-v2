/**
 * Bundles the custom-page browser test's outer page (harness.tsx) into e2e/custom-page/dist/,
 * which `scripts/browser-e2e/run.ts` runs before it copies e2e/ for the browser container; the
 * test serves the bundle itself (custom-page.e2e.ts). Usage, from the repository root:
 *
 *   bun frontend/e2e/custom-page/build.ts
 */
import { rmSync } from "node:fs";
import { join } from "node:path";

export const HARNESS_DIST = join(import.meta.dir, "dist");

export async function buildHarness(): Promise<void> {
  rmSync(HARNESS_DIST, { recursive: true, force: true });
  const result = await Bun.build({
    entrypoints: [join(import.meta.dir, "harness.tsx")],
    outdir: HARNESS_DIST,
    target: "browser",
    format: "esm",
    naming: "harness.js",
    // The SPA's CSP has no 'unsafe-eval'; the bundle needs none.
    minify: false,
    define: { "process.env.NODE_ENV": JSON.stringify("development") },
  });
  if (!result.success) {
    throw new AggregateError(result.logs, "bundling the custom-page harness failed");
  }
}

if (import.meta.main) {
  await buildHarness();
  process.stdout.write(`built ${join(HARNESS_DIST, "harness.js")}\n`);
}
