import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

/**
 * `react-style-singleton` writes Radix's scroll-lock rules into a `<style>` element, which the
 * CSP refuses; this module stands in for it and src/styles/global.css holds the rule instead
 * (frontend/README.md, 「CSP 的限制」).
 */
export const STYLE_SINGLETON_ALIAS = {
  find: /^react-style-singleton$/,
  replacement: fileURLToPath(new URL("./src/lib/no-style-singleton.ts", import.meta.url)),
};

/**
 * The conversation's Markdown parser (micromark, under assistant-ui's Markdown and react-markdown)
 * decodes named character references with `decode-named-character-reference`. Its browser build
 * writes the reference into an element's `innerHTML` and reads it back, an HTML sink that Trusted
 * Types refuses; its default build looks the name up in a table. The build takes the default one,
 * as Node resolves it (frontend/README.md, 「CSP 的限制」).
 */
export const CHARACTER_REFERENCE_ALIAS = {
  find: /^decode-named-character-reference$/,
  replacement: createRequire(import.meta.url).resolve("decode-named-character-reference"),
};

/**
 * Packages that change only when a dependency is upgraded, never with application code. They go
 * into one `framework` chunk, so its hash-only file name stays the same across deployments of new
 * application code and a browser that has it cached downloads only the application chunks again
 * (Laysi/trustiot-v2#977). Packages whose content follows the interface (lucide-react's icons,
 * the Radix primitives) stay with the code that uses them.
 */
export const FRAMEWORK_PACKAGES = [
  "react",
  "react-dom",
  "scheduler",
  "react-router",
  "@tanstack/react-query",
  "@tanstack/query-core",
  "zod",
  "openapi-fetch",
  "tailwind-merge",
];

/** Matches a module of one of those packages (not a package whose name merely starts the same). */
export const FRAMEWORK_MODULE = new RegExp(
  `[\\\\/]node_modules[\\\\/](?:${FRAMEWORK_PACKAGES.map((name) => name.replace("/", "[\\\\/]")).join("|")})[\\\\/]`,
);

/**
 * The build layout is a contract with services/web, which serves dist/ (frontend/README.md):
 * index.html and the files of public/ at the root, everything Vite emits under /static/ with
 * hash-only names. Absolute URLs (base "/") let a deep link such as /devices/42 load the same
 * assets as /. Nothing is inlined: no data: URIs and no inline script or style, so the CSP can
 * stay at 'self' (tools/check-dist.ts fails the build otherwise). Tailwind compiles to one static
 * stylesheet at build time; nothing generates styles in the browser.
 */
export default defineConfig(({ command }) => ({
  base: "/",
  plugins: [react(), tailwindcss()],
  resolve: { alias: [STYLE_SINGLETON_ALIAS, CHARACTER_REFERENCE_ALIAS] },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "static",
    assetsInlineLimit: 0,
    sourcemap: false,
    // The polyfill is an inline script; every browser we support has modulepreload.
    modulePreload: { polyfill: false },
    rolldownOptions: {
      output: {
        // Hash-only names: no module or file names from the source end up in the image.
        entryFileNames: "static/[hash].js",
        chunkFileNames: "static/[hash].js",
        assetFileNames: "static/[hash][extname]",
        codeSplitting: { groups: [{ name: "framework", test: FRAMEWORK_MODULE }] },
      },
    },
  },
  server: command === "serve" ? { strictPort: true, proxy: devProxy(process.env.TRUSTIOT_DEV_API_ORIGIN) } : undefined,
}));

/**
 * In development, /api and /auth go to the services on the host (scripts/README.md), so the SPA
 * and the API share one origin as they do behind the ingress. The Host header is kept, so the
 * identity service's same-origin check compares it with the page's own origin.
 */
function devProxy(origin: string | undefined) {
  if (!origin) {
    return undefined;
  }
  return {
    "/api": { target: origin, changeOrigin: false },
    "/auth": { target: origin, changeOrigin: false },
  };
}
