// The browser check of the UI preview (Laysi/trustiot-v2#1065), run once by hand in the Playwright
// image (frontend/e2e/image.ts) against `bun run --cwd frontend ui-preview` on 127.0.0.1:5174.
// It opens each page the issue names, waits for the fake data, and prints one JSON record per page
// (the page's heading, the menu, texts it looked for, the console errors and every request that
// left the page for /api or /auth: the fakes answer in the page, so there must be none).
//
// Usage (repository root, the preview running):
//   podman run --rm --network host --shm-size 1g -v "$PWD:/w:Z" -w /w <PLAYWRIGHT_IMAGE> \
//     node docs/test-reports/data/2026-11-04-1065-ui-preview/browser-check.mjs
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
const WAIT_MS = 60_000;

const browser = await chromium.launch();
const context = await browser.newContext({ locale: "en-US", timezoneId: "Asia/Taipei", viewport: { width: 1280, height: 800 } });
const page = await context.newPage();
const consoleErrors = [];
const networkApi = [];
page.on("console", (message) => {
  if (message.type() === "error") consoleErrors.push(message.text());
});
page.on("pageerror", (error) => consoleErrors.push(`pageerror: ${error.message}`));
page.on("request", (request) => {
  const { pathname } = new URL(request.url());
  if (pathname.startsWith("/api/") || pathname.startsWith("/auth/")) networkApi.push(`${request.method()} ${pathname}`);
});

async function menuEntries() {
  await page.getByRole("button", { name: "Menu" }).click();
  const items = page.getByRole("menu").getByRole("menuitem");
  await items.first().waitFor({ timeout: WAIT_MS });
  const entries = await items.evaluateAll((nodes) => nodes.map((node) => `${node.textContent} ${node.getAttribute("href")}`));
  await page.keyboard.press("Escape");
  return entries;
}

async function record(name, path, expected, open) {
  consoleErrors.length = 0;
  networkApi.length = 0;
  if (path !== null) await page.goto(`${BASE}${path}`);
  if (open) await open();
  for (const text of expected) await page.getByText(text).first().waitFor({ timeout: WAIT_MS });
  // Buttons only an editor or admin has (the dashboard page's archive).
  const archiveButtons = await page.getByRole("button", { name: "Archive", exact: true }).count();
  const result = {
    page: name,
    url: new URL(page.url()).pathname,
    toolbar: await page.getByRole("region", { name: "UI preview" }).isVisible(),
    heading: (await page.locator("main h1").first().textContent())?.trim() ?? null,
    found: expected.map(String),
    archiveButtons,
    menu: await menuEntries(),
    consoleErrors: [...consoleErrors],
    requestsToNetworkApi: [...networkApi],
  };
  console.log(JSON.stringify(result));
  return result;
}

// The language is the browser's (en-US): the fake user's preference is "auto".
await record("home", "/", ["Line 2 energy this week"]);
await record("conversations", "/conversations", ["Cold room temperature alarms"]);
await record("dashboards", "/dashboards", ["Plant energy overview", "Energy by site"]);
await record("dashboard (admin)", null, ["Energy by site"], async () => {
  await page.getByRole("link", { name: "Energy by site" }).first().click();
  await page.getByRole("heading", { level: 1, name: "Energy by site" }).waitFor({ timeout: WAIT_MS });
  await page.getByRole("button", { name: "Archive", exact: true }).waitFor({ timeout: WAIT_MS });
});
await record("devices", "/devices", ["line-2-meter", "compressor-1"]);
await record("device", null, ["temperature"], async () => {
  await page.getByRole("link", { name: "cold-room-1" }).first().click();
});
await record("settings", "/settings", ["lin.mei@example.com"]);

// The toolbar's role: viewer reloads the page with the fakes created again for a viewer.
await page.goto(`${BASE}/dashboards`);
await Promise.all([page.waitForEvent("load"), page.getByLabel("Role").selectOption("viewer")]);
const viewer = await record("dashboard (viewer)", null, ["Energy by site"], async () => {
  await page.getByRole("link", { name: "Energy by site" }).first().click();
  await page.getByRole("heading", { level: 1, name: "Energy by site" }).waitFor({ timeout: WAIT_MS });
});
console.log(JSON.stringify({ check: "viewer has no Archive button", archiveButtons: viewer.archiveButtons }));

// Failures armed in the toolbar, on every request (the page also refreshes other data when it
// opens, so "the next request" may be one of those; the unit tests check that one failure is used
// up by one request). 500 is not retried; 503 is retried by the query client and then shown.
for (const [kind, menuEntry, expected] of [
  ["internal", "Devices", /couldn.t be loaded/],
  ["unavailable", "Saved dashboards", /temporarily unavailable|couldn.t be loaded/],
]) {
  await page.goto(`${BASE}/settings`);
  await page.getByText("lin.mei@example.com").waitFor({ timeout: WAIT_MS });
  await page.getByLabel("Every request").check();
  await page.getByLabel("Fail next request").selectOption(kind);
  await page.getByRole("button", { name: "Menu" }).click();
  await page.getByRole("menuitem", { name: menuEntry }).click();
  await record(`${menuEntry} (${kind} on every request)`, null, [expected]);
  await page.getByLabel("Fail next request").selectOption("");
}

// The empty seed: the fakes are created again without data.
await Promise.all([page.waitForEvent("load"), page.getByLabel("Role").selectOption("admin")]);
await Promise.all([page.waitForEvent("load"), page.getByLabel("Data").selectOption("empty")]);
await record("devices (empty)", "/devices", ["No devices yet"]);

await browser.close();
