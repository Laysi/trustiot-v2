import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

/**
 * The build layout is a contract with services/web, which serves dist/ (frontend/README.md):
 * index.html and the files of public/ at the root, everything Vite emits under /static/ with
 * hash-only names. Absolute URLs (base "/") let a deep link such as /devices/42 load the same
 * assets as /. Nothing is inlined: no data: URIs and no inline script or style, so the CSP can
 * stay at 'self' (tools/check-dist.ts fails the build otherwise).
 */
export default defineConfig(({ command }) => ({
  base: "/",
  plugins: [react()],
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
