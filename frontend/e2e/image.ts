// The browser the end-to-end tests run in: the Playwright image of the same release as the
// `@playwright/test` package in package.json and bun.lock, pinned by the digest of its multi-arch
// manifest (linux/amd64 and linux/arm64), so the Chromium build is fixed too. The image is
// written as `name:tag@digest` in one literal, which Renovate reads and moves together with the
// packages (renovate.json5, group `playwright`); `pins.test.ts` fails when the two differ
// (frontend/README.md, 「瀏覽器端對端測試」).
export const PLAYWRIGHT_IMAGE =
  "mcr.microsoft.com/playwright:v1.64.1-noble@sha256:47b510dd9230bf0b9bfc9b20102241e577ea7a99334ab4c3ca19f19770e9b8ea";

/** The Playwright release the image holds, from its tag. */
export const PLAYWRIGHT_VERSION = /:v(\d+\.\d+\.\d+)-noble@/.exec(PLAYWRIGHT_IMAGE)?.[1] ?? "";
