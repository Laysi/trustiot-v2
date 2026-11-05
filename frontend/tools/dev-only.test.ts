import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { build, type Plugin } from "vite";

import { checkDist, readDist } from "./check-dist.ts";
import { DEV_ONLY_MARKER, devOnlyMarker, FRONTEND_ROOT, isDevOnlyModule } from "./dev-only.ts";
import { previewEntryFor, UI_PREVIEW_ENTRY } from "./ui-preview-server.ts";

const CONFIG = fileURLToPath(new URL("../vite.config.ts", import.meta.url));
const ENTRY = fileURLToPath(new URL("../src/main.tsx", import.meta.url));
/** A real build takes a few seconds, more on a loaded machine; nothing here depends on how long. */
const BUILD_TIMEOUT_MS = 180_000;

const at = (path: string) => join(FRONTEND_ROOT, path);

describe("development-only modules", () => {
  test.each([
    "src/test/fake-devices.ts",
    "src/test/render.tsx",
    "src/ui-preview/main.tsx",
    "src/api/pending/members.ts",
    "src/pages/members/preview.ts",
    "src/pages/platform/tenants/preview.tsx",
    "src/app/menu.test.ts",
  ])("%s is development-only", (path) => {
    expect(isDevOnlyModule(at(path))).toBe(true);
    expect(isDevOnlyModule(`${at(path)}?import`)).toBe(true);
  });

  test.each([
    "src/main.tsx",
    "src/api/devices.ts",
    "src/pages/device/DevicesPage.tsx",
    "src/pages/previewer.ts",
    "src/dashboard/fixtures.ts",
    "node_modules/react/index.js",
  ])("%s is not", (path) => {
    expect(isDevOnlyModule(at(path))).toBe(false);
  });

  test("the build marks a development-only module and leaves the others alone", () => {
    const plugin = devOnlyMarker();
    const transform = plugin.transform as (code: string, id: string) => { code: string } | null;
    expect(transform("export const a = 1;", at("src/test/fake-x.ts"))?.code).toContain(DEV_ONLY_MARKER);
    expect(transform("export const a = 1;", at("src/api/devices.ts"))).toBeNull();
    expect(plugin.apply).toBe("build");
  });
});

describe("the UI preview's dev server", () => {
  test("a page load of a client route is served from the preview's entry", () => {
    expect(previewEntryFor("GET", "text/html,*/*", "/")).toBe(UI_PREVIEW_ENTRY);
    expect(previewEntryFor("GET", "text/html", "/devices/42?x=1")).toBe(UI_PREVIEW_ENTRY);
    expect(previewEntryFor("HEAD", "text/html", "/settings")).toBe(UI_PREVIEW_ENTRY);
  });

  test("modules, files and Vite's own paths are served as they are", () => {
    expect(previewEntryFor("GET", "*/*", "/devices")).toBeNull();
    expect(previewEntryFor("GET", "text/html", "/src/main.tsx")).toBeNull();
    expect(previewEntryFor("GET", "text/html", "/@vite/client")).toBeNull();
    expect(previewEntryFor("GET", "text/html", "/favicon.svg")).toBeNull();
    expect(previewEntryFor("POST", "text/html", "/devices")).toBeNull();
  });
});

const directories: string[] = [];

afterAll(() => {
  for (const directory of directories) rmSync(directory, { recursive: true, force: true });
});

describe("a fake mixed into the build", () => {
  test(
    "check:dist refuses the output",
    async () => {
      const outDir = mkdtempSync(join(tmpdir(), "trustiot-dev-only-"));
      directories.push(outDir);
      // A production module that imports a fake, as a mistake in a page would; its export is unused.
      const mixIn: Plugin = {
        name: "mix-in-a-fake",
        transform(code, id) {
          return id === ENTRY ? `import "./test/fake-devices.ts";\n${code}` : null;
        },
      };
      await build({ configFile: CONFIG, logLevel: "silent", plugins: [mixIn], build: { outDir, emptyOutDir: true } });
      const problems = checkDist(readDist(outDir), new Set(readdirSync(join(FRONTEND_ROOT, "public"))), new Set());
      expect(problems.filter((problem) => problem.includes("development-only code")).length).toBeGreaterThan(0);
    },
    BUILD_TIMEOUT_MS,
  );
});
