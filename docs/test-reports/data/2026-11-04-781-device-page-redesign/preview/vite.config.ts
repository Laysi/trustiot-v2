import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

import { CHARACTER_REFERENCE_ALIAS, FRAMEWORK_MODULE, STYLE_SINGLETON_ALIAS } from "../vite.config.ts";

// One-off preview build (not committed): the SPA's real App and routes with the tests' in-memory
// servers behind them, built the way the product is (static files, no inline script or style).
export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  publicDir: fileURLToPath(new URL("../public", import.meta.url)),
  base: "/",
  plugins: [react(), tailwindcss()],
  resolve: { alias: [STYLE_SINGLETON_ALIAS, CHARACTER_REFERENCE_ALIAS] },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "static",
    assetsInlineLimit: 0,
    sourcemap: false,
    modulePreload: { polyfill: false },
    rolldownOptions: {
      output: {
        entryFileNames: "static/[hash].js",
        chunkFileNames: "static/[hash].js",
        assetFileNames: "static/[hash][extname]",
        codeSplitting: { groups: [{ name: "framework", test: FRAMEWORK_MODULE }] },
      },
    },
  },
});
