/**
 * The dev server of the UI preview (`bun run --cwd frontend ui-preview`, `vite --mode ui-preview`;
 * frontend/README.md, 「UI 預覽」). The SPA's routes are client routes, so the dev server answers a
 * page load of any of them (`/devices/42`) with an HTML entry: in the preview that entry is
 * ui-preview.html instead of index.html, so a reload or a deep link stays in the preview.
 */

import type { Plugin } from "vite";

/** The Vite mode of the UI preview. */
export const UI_PREVIEW_MODE = "ui-preview";
/** Its own port, so it runs next to `dev` and keeps its own browser storage (another origin). */
export const UI_PREVIEW_PORT = 5174;
export const UI_PREVIEW_ENTRY = "/ui-preview.html";

/**
 * The entry a request is served from instead of its own URL, or `null` to serve it as it is: a
 * page load (GET or HEAD that accepts HTML) of a path without a file extension, outside Vite's own
 * `/@…` and `/__…` paths.
 */
export function previewEntryFor(method: string | undefined, accept: string | undefined, url: string | undefined) {
  if ((method !== "GET" && method !== "HEAD") || !accept?.includes("text/html") || url === undefined) return null;
  const pathname = url.split(/[?#]/)[0] ?? "";
  if (!pathname.startsWith("/") || pathname.startsWith("/@") || pathname.startsWith("/__")) return null;
  const last = pathname.slice(pathname.lastIndexOf("/") + 1);
  return last.includes(".") ? null : UI_PREVIEW_ENTRY;
}

export function uiPreviewEntry(): Plugin {
  return {
    name: "trustiot-ui-preview-entry",
    apply: (_config, env) => env.command === "serve" && env.mode === UI_PREVIEW_MODE,
    configureServer(server) {
      // Added before Vite's own middlewares, so its SPA fallback to index.html never sees these.
      server.middlewares.use((request, _response, next) => {
        const entry = previewEntryFor(request.method, request.headers.accept, request.url);
        if (entry !== null) request.url = entry;
        next();
      });
    },
  };
}
