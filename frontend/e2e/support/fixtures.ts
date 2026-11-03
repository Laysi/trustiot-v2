// What every test gets: the control client, and a watch on the page that collects what a test
// has to check after each step (frontend/README.md, 「CSP 的限制」): the browser's
// `securitypolicyviolation` events (the enforced policy and the Trusted Types report-only one),
// and the console's errors.
import { type APIRequestContext, test as base, expect, type Page, request } from "@playwright/test";

import { type Control, controlFromEnvironment, SESSION_COOKIE } from "./control.ts";

declare global {
  interface Window {
    /** Filled by the init script below: one line per `securitypolicyviolation` event. */
    __cspViolations?: string[];
  }
}

/**
 * The browser logs "Failed to load resource: the server responded with a status of 401" as a
 * console error for every refused request. The tests that expect a refusal check its status on
 * the response, so these lines say nothing more and are not counted.
 */
const RESOURCE_STATUS_LINE = /^Failed to load resource: the server responded with a status of \d{3}\b/;

export interface Watch {
  /** Console errors and uncaught page errors since the page opened (refused-request lines left out). */
  problems(): readonly string[];
  /** Drops what was collected: for a test that causes a console error on purpose. */
  forget(): void;
  /** The CSP violations the current document has seen. */
  violations(page: Page): Promise<string[]>;
}

export interface Fixtures {
  control: Control;
  watch: Watch;
}

export const test = base.extend<Fixtures>({
  // biome-ignore lint/correctness/noEmptyPattern: Playwright reads the fixtures a function names from this pattern
  control: async ({}, use) => {
    await use(controlFromEnvironment());
  },
  watch: [
    async ({ page }, use) => {
      await page.addInitScript(() => {
        const seen: string[] = [];
        window.__cspViolations = seen;
        document.addEventListener("securitypolicyviolation", (event) => {
          seen.push(`${event.disposition} ${event.effectiveDirective} ${event.blockedURI}`);
        });
      });
      const problems: string[] = [];
      page.on("console", (message) => {
        if (message.type() === "error" && !RESOURCE_STATUS_LINE.test(message.text())) {
          problems.push(`console.error: ${message.text()}`);
        }
      });
      page.on("pageerror", (error) => {
        problems.push(`pageerror: ${error.message}`);
      });
      await use({
        problems: () => problems,
        forget: () => {
          problems.length = 0;
        },
        violations: (target) => target.evaluate(() => window.__cspViolations ?? []),
      });
      // Whatever the test did not look at itself.
      expect(problems, "console errors and uncaught errors during the test").toEqual([]);
    },
    { auto: true },
  ],
});

export { expect } from "@playwright/test";

/** Puts a session cookie into the page's browser (Chromium keeps a `__Host-` cookie on http://localhost). */
export async function signInWithCookie(page: Page, cookie: string): Promise<void> {
  await page
    .context()
    .addCookies([{ name: SESSION_COOKIE, value: cookie, domain: "localhost", path: "/", secure: true }]);
}

/**
 * A second device: requests with that session's cookie, outside the page's browser. The request
 * says it is same-origin the way a browser's fetch would (Sec-Fetch-Site; services/identity/README.md
 * 「跨站請求」), because the identity service refuses a write that does not.
 */
export async function otherDevice(baseURL: string, cookie: string): Promise<APIRequestContext> {
  return request.newContext({
    baseURL,
    extraHTTPHeaders: { cookie: `${SESSION_COOKIE}=${cookie}`, "sec-fetch-site": "same-origin" },
  });
}
