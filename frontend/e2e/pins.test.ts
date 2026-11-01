// The browser the end-to-end tests use is fixed by two things that must agree: the
// `@playwright/test` package (package.json, bun.lock) and the Playwright image (image.ts), whose
// Chromium belongs to that release. A pull request that moves only one of them fails here.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { PLAYWRIGHT_IMAGE, PLAYWRIGHT_VERSION } from "./image.ts";

const packageJson = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as {
  devDependencies: Record<string, string>;
};
const lock = readFileSync(new URL("../bun.lock", import.meta.url), "utf8");

describe("the browser of the end-to-end tests", () => {
  test("@playwright/test is an exact version, the one the image is of", () => {
    expect(PLAYWRIGHT_VERSION).toMatch(/^\d+\.\d+\.\d+$/);
    expect(packageJson.devDependencies["@playwright/test"]).toBe(PLAYWRIGHT_VERSION);
  });

  test("bun.lock resolves playwright and playwright-core to that version", () => {
    for (const name of ["@playwright/test", "playwright", "playwright-core"]) {
      expect(lock, name).toContain(`"${name}": ["${name}@${PLAYWRIGHT_VERSION}"`);
    }
  });

  test("the image is that release's tag, pinned by the digest of its manifest", () => {
    expect(PLAYWRIGHT_IMAGE.startsWith(`mcr.microsoft.com/playwright:v${PLAYWRIGHT_VERSION}-noble@sha256:`)).toBe(true);
    expect(PLAYWRIGHT_IMAGE).toMatch(/@sha256:[0-9a-f]{64}$/);
    expect(PLAYWRIGHT_IMAGE).not.toContain("latest");
  });
});
