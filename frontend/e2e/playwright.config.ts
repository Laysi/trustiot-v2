// Playwright's configuration for the browser end-to-end tests. They run in the Playwright
// container that `scripts/browser-e2e/run.ts` starts, with the addresses of the environment it
// made in the variables below (frontend/README.md, 「瀏覽器端對端測試」); outside that run there is
// nothing for them to talk to, so a missing variable is an error and not a silent skip.
import { defineConfig, devices } from "@playwright/test";

function required(name: string): string {
  const value = process.env[name];
  if (value === undefined || value === "") {
    throw new Error(`${name} is not set: run the tests with \`bun scripts/browser-e2e/run.ts\` (frontend/README.md)`);
  }
  return value;
}

const out = process.env.E2E_OUT ?? "test-results";

export default defineConfig({
  testDir: ".",
  // Not `.test.ts` or `.spec.ts`: `bun test` runs those, and these need the environment.
  testMatch: "**/*.e2e.ts",
  // The tests share one identity service and one database; each makes its own users.
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: true,
  // A generous bound for the test as a whole; every wait inside it names what it waits for.
  timeout: 180_000,
  expect: { timeout: 30_000 },
  outputDir: `${out}/artifacts`,
  reporter: [["list"], ["json", { outputFile: `${out}/results.json` }]],
  use: {
    baseURL: required("E2E_BASE_URL"),
    locale: "en-US",
    timezoneId: "UTC",
    actionTimeout: 30_000,
    navigationTimeout: 60_000,
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
