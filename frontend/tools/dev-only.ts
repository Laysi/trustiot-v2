/**
 * The code that exists only for development and must never reach the build that is served: the
 * test fakes (src/test/), the UI preview (src/ui-preview/, ui-preview.html), the interfaces of APIs
 * whose backend does not exist yet (src/api/pending/) and the pages' preview routes
 * (src/pages/<area>/preview.ts). frontend/README.md, 「UI 預覽」.
 *
 * The build names its files by hash only, so the output cannot be searched for module names.
 * Instead `devOnlyMarker()` (a Vite plugin, build only) appends a statement with
 * `DEV_ONLY_MARKER` to every such module that the build takes in. The statement writes a global,
 * so the bundler keeps it (and with it the module) even when nothing uses the module's exports,
 * and the minifier keeps the property name; tools/check-dist.ts then fails on any output file that
 * contains the marker.
 */

import { relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

import type { Plugin } from "vite";

export const FRONTEND_ROOT = fileURLToPath(new URL("..", import.meta.url));

/** The name of the global the marker statement writes; it appears nowhere else in the source of the SPA. */
export const DEV_ONLY_MARKER = "__TRUSTIOT_DEV_ONLY_MODULE__";

const DEV_ONLY_PATHS: readonly RegExp[] = [
  /^src\/test\//,
  /^src\/ui-preview\//,
  /^src\/api\/pending\//,
  /^src\/pages\/(?:.+\/)?preview\.tsx?$/,
  /^ui-preview\.html$/,
  /\.test\.tsx?$/,
];

/** Whether the module `id` (an absolute file path, perhaps with a `?query`) is development-only code. */
export function isDevOnlyModule(id: string, root: string = FRONTEND_ROOT): boolean {
  const file = id.split("?")[0] ?? "";
  const path = relative(root, file).split(sep).join("/");
  if (path.startsWith("../") || path.startsWith("node_modules/")) return false;
  return DEV_ONLY_PATHS.some((pattern) => pattern.test(path));
}

/** The statement appended to a development-only module in a build. */
export const MARKER_STATEMENT = `\n;globalThis.${DEV_ONLY_MARKER} = true;\n`;

/** Marks every development-only module a build takes in, so check:dist finds it in the output. */
export function devOnlyMarker(root: string = FRONTEND_ROOT): Plugin {
  return {
    name: "trustiot-dev-only-marker",
    apply: "build",
    transform(code, id) {
      if (!isDevOnlyModule(id, root) || !/\.[cm]?[jt]sx?$/.test(id.split("?")[0] ?? "")) return null;
      return { code: `${code}${MARKER_STATEMENT}`, map: null };
    },
  };
}
