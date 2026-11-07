// The browser check of the dashboard source panel (Laysi/trustiot-v2#1175), run in the Playwright
// image (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`) on 127.0.0.1:5174.
// It opens the panel for an SQL dataset, a statistics dataset and a usage dataset on the dashboard
// page, copies the query with and without the clipboard, asks the assistant to explain, looks at
// the panel inside a conversation, and repeats the main views in the dark theme and at 390 px.
// Every step prints one JSON record of DOM text (no screenshots).
//
// Usage: docs/test-reports/data/2026-11-07-1175-source-panel/run.sh (from the repository root).
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
const WAIT_MS = 60_000;
const CONVERSATION = "/conversations/3e7b1c94-8a52-4d60-b9f3-0c6d2a5e7f18";

const browser = await chromium.launch();
const failures = [];
const consoleErrors = [];

async function newPage(options = {}) {
  const context = await browser.newContext({
    locale: "en-US",
    timezoneId: "Asia/Taipei",
    viewport: { width: 1280, height: 800 },
    ...options.context,
  });
  if (options.permissions) await context.grantPermissions(options.permissions, { origin: BASE });
  if (options.init) await context.addInitScript(options.init);
  const page = await context.newPage();
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text());
  });
  page.on("pageerror", (error) => consoleErrors.push(`pageerror: ${error.message}`));
  return { context, page };
}

function emit(step, data) {
  console.log(JSON.stringify({ step, ...data, consoleErrors: [...consoleErrors] }));
  consoleErrors.length = 0;
}

function check(step, condition, what) {
  if (!condition) failures.push(`${step}: ${what}`);
}

/** The card titled `title` and its "Source of <title>" button's panel, opened. */
async function openPanel(page, title) {
  const card = page.getByRole("heading", { level: 3, name: title }).locator("xpath=ancestor::section[1]");
  await card.waitFor({ timeout: WAIT_MS });
  await card.getByRole("button", { name: `Source of ${title}` }).click();
  const panel = card.getByRole("region", { name: "Source" });
  await panel.waitFor({ timeout: WAIT_MS });
  // The facts come with the dataset's result.
  await panel.getByText("Rows", { exact: true }).waitFor({ timeout: WAIT_MS });
  return panel;
}

/** What the panel says, as DOM text. */
async function readPanel(panel) {
  return panel.evaluate((root) => {
    const text = (node) => node?.textContent?.replace(/\s+/g, " ").trim() ?? null;
    return {
      heading: text(root.querySelector("h4")),
      lines: [...root.querySelectorAll("dt")].map((dt) => ({ label: text(dt), value: text(dt.nextElementSibling) })),
      groups: [...root.querySelectorAll("fieldset > legend")].map((legend) => text(legend)),
      pre: [...root.querySelectorAll("pre")].map((pre) => ({
        focusable: pre.tabIndex === 0,
        text: pre.textContent,
      })),
      buttons: [...root.querySelectorAll("button")].map((button) => button.getAttribute("aria-label") ?? text(button)),
      status: { role: root.querySelector("[role=status]")?.getAttribute("role") ?? null, text: text(root.querySelector("[role=status]")) },
      background: getComputedStyle(root).backgroundColor,
      color: getComputedStyle(root).color,
    };
  });
}

const fact = (read, label) => read.lines.find((line) => line.label === label)?.value ?? null;

async function pageMetrics(page) {
  return page.evaluate(() => ({
    theme: document.documentElement.dataset.theme ?? null,
    prefersDark: matchMedia("(prefers-color-scheme: dark)").matches,
    pageBackground: getComputedStyle(document.body).backgroundColor,
    scrollWidth: document.documentElement.scrollWidth,
    clientWidth: document.documentElement.clientWidth,
    // The app itself: the preview's own toolbar (dev only, before #root) is wider than a phone.
    rootScrollWidth: document.getElementById("root")?.scrollWidth ?? null,
    rootClientWidth: document.getElementById("root")?.clientWidth ?? null,
  }));
}

/** The app's elements (not the preview's toolbar) that reach past the window's right edge, widest first. */
async function overflowers(page) {
  return page.evaluate(() => {
    const limit = document.documentElement.clientWidth;
    return [...document.querySelectorAll("#root *")]
      .map((element) => ({ element, right: Math.round(element.getBoundingClientRect().right) }))
      .filter(({ right }) => right > limit)
      .sort((a, b) => b.right - a.right)
      .slice(0, 6)
      .map(({ element, right }) => ({
        right,
        tag: element.tagName.toLowerCase(),
        label: element.getAttribute("aria-label") ?? element.getAttribute("data-testid") ?? element.className?.toString().slice(0, 60),
        text: element.textContent?.replace(/\s+/g, " ").trim().slice(0, 40),
      }));
  });
}

async function openDashboard(page, title) {
  await page.goto(`${BASE}/dashboards`);
  try {
    await page.getByRole("link", { name: title }).first().waitFor({ timeout: WAIT_MS });
  } catch (error) {
    console.log(JSON.stringify({ step: "dashboard list without the dashboard", title, body: (await page.locator("main").textContent())?.slice(0, 600), consoleErrors }));
    throw error;
  }
  await page.getByRole("link", { name: title }).first().click();
  await page.getByRole("heading", { level: 1, name: title }).waitFor({ timeout: WAIT_MS });
}

async function copyStatus(page, panel, buttonName, expected) {
  await panel.getByRole("button", { name: buttonName }).click();
  const status = panel.getByRole("status");
  await page.waitForFunction(
    ({ selector, text }) => document.querySelector(selector)?.textContent === text,
    { selector: "[role=status]:not(:empty)", text: expected },
    { timeout: WAIT_MS },
  ).catch(() => {});
  return (await status.textContent())?.trim() ?? "";
}

// 1. Light, 1280: the statistics panel, with clipboard permission.
{
  const { context, page } = await newPage({ permissions: ["clipboard-read", "clipboard-write"] });
  await openDashboard(page, "Line 2 power statistics");
  const panel = await openPanel(page, "Daily average power");
  const read = await readPanel(panel);
  emit("statistics panel (light, 1280)", { read, metrics: await pageMetrics(page) });
  check("statistics panel", fact(read, "Period") === "The last 7 days", "period text");
  check("statistics panel", fact(read, "Grouped by") === "Every day", "interval text");
  check("statistics panel", fact(read, "Time zone") === "Asia/Taipei", "time zone");
  check("statistics panel", read.lines.some((line) => line.label === "Data range"), "data range present");
  check("statistics panel", fact(read, "Rows") === "4", "row count");
  check("statistics panel", read.pre.some((pre) => pre.text.includes('"period": "last_7_days"')), "raw parameters present");

  // Device names: the first series is listed by the device list, the second is not.
  await page.waitForFunction(
    () => document.querySelector("[data-testid=request-summary]")?.textContent?.includes("line-2-meter"),
    null,
    { timeout: WAIT_MS },
  );
  const points = await panel.getByTestId("request-summary").getByRole("listitem").allTextContents();
  emit("statistics panel data points", { points });
  check("device names", points[0]?.startsWith("line-2-meter"), "listed device by name");
  check("device names", points[1]?.startsWith("0b8a2e91-5c46-4d17-8f03-6a9d7e1c2b48"), "unlisted device by ID");

  // Copy with clipboard permission: the raw parameters are on the clipboard and the status says so.
  const status = await copyStatus(page, panel, "Copy the parameters", "Copied the query parameters.");
  const clipboardText = await page.evaluate(() => navigator.clipboard.readText());
  const raw = read.pre.find((pre) => pre.text.includes("period"))?.text;
  emit("copy parameters (clipboard permission granted)", { status, clipboardEqualsRawParameters: clipboardText === raw });
  check("copy parameters", status === "Copied the query parameters.", "success status");
  check("copy parameters", clipboardText === raw, "clipboard equals the raw parameters");

  // Ask AI to explain: a new conversation with the dataset and the dashboard named, and a way back.
  await panel.getByRole("button", { name: "Ask AI to explain" }).click();
  await page.waitForURL(/\/conversations\/[0-9a-f-]{36}$/, { timeout: WAIT_MS });
  const back = page.getByRole("link", { name: "Back to Line 2 power statistics" });
  await back.waitFor({ timeout: WAIT_MS });
  await page.getByText("Explain where", { exact: false }).first().waitFor({ timeout: WAIT_MS });
  const asked = await page.getByText("Explain where", { exact: false }).first().textContent();
  const backHref = await back.getAttribute("href");
  emit("ask AI to explain", { url: new URL(page.url()).pathname, asked, backLink: await back.textContent(), backHref });
  check("ask AI", asked?.includes("“Line 2 average power”") && asked.includes("“Line 2 power statistics”"), "question names dataset and dashboard");
  await back.click();
  await page.getByRole("heading", { level: 1, name: "Line 2 power statistics" }).waitFor({ timeout: WAIT_MS });
  emit("back to the dashboard", { url: new URL(page.url()).pathname });
  check("back", new URL(page.url()).pathname === backHref, "back link returns to the dashboard");

  // The usage query with an explicit range.
  const usage = await openPanel(page, "Energy by week");
  const usageRead = await readPanel(usage);
  emit("usage panel with an explicit range", { read: usageRead });
  check("usage panel", /^Oct 12, 2026.* to Nov 2, 2026/.test(fact(usageRead, "Period") ?? "") || (fact(usageRead, "Period") ?? "").includes(" to "), "explicit period");
  check("usage panel", fact(usageRead, "Grouped by") === "Every week", "interval text");
  await context.close();
}

// 2. Copy failure: the browser refuses, and there is no clipboard at all.
for (const [name, init] of [
  ["browser refuses", () => { Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: () => Promise.reject(new DOMException("denied", "NotAllowedError")) } }); }],
  ["no clipboard", () => { Object.defineProperty(navigator, "clipboard", { configurable: true, value: undefined }); }],
]) {
  const { context, page } = await newPage({ init });
  await openDashboard(page, "Line 2 power statistics");
  const panel = await openPanel(page, "Daily average power");
  const status = await copyStatus(page, panel, "Copy the parameters", "Couldn't copy. Select the text and copy it yourself.");
  const role = await panel.getByRole("status").getAttribute("role");
  emit(`copy failure (${name})`, { status, role });
  check(`copy failure ${name}`, status === "Couldn't copy. Select the text and copy it yourself.", "failure status");
  await context.close();
}

// 3. An SQL dataset: facts, copy and the Ask AI button.
{
  const { context, page } = await newPage({ permissions: ["clipboard-read", "clipboard-write"] });
  await openDashboard(page, "Plant energy overview");
  const panel = await openPanel(page, "Power by line");
  const read = await readPanel(panel);
  emit("SQL panel (light, 1280)", { read, metrics: await pageMetrics(page) });
  check("SQL panel", read.lines.some((line) => line.label === "Queried at"), "queried at");
  check("SQL panel", read.lines.some((line) => line.label === "Data range"), "data range");
  check("SQL panel", read.lines.some((line) => line.label === "Rows"), "rows");
  check("SQL panel", read.groups.includes("The query's SQL") && read.pre.some((pre) => pre.focusable), "named, focusable SQL block");
  check("SQL panel", read.buttons.includes("Ask AI to explain"), "Ask AI button on the dashboard page");
  const status = await copyStatus(page, panel, "Copy the SQL", "Copied the SQL.");
  const clipboardText = await page.evaluate(() => navigator.clipboard.readText());
  emit("copy SQL (clipboard permission granted)", { status, clipboardIsTheSql: read.pre.some((pre) => pre.text === clipboardText) });
  check("copy SQL", status === "Copied the SQL." && read.pre.some((pre) => pre.text === clipboardText), "SQL copied");
  await context.close();
}

// 4. Inside a conversation: the answer's dashboard has the facts and the copy button, no Ask AI.
{
  const { context, page } = await newPage({ permissions: ["clipboard-read", "clipboard-write"] });
  await page.goto(`${BASE}${CONVERSATION}`);
  const article = page.getByRole("article", { name: "TrustIoT's answer" });
  await article.waitFor({ timeout: WAIT_MS });
  const panel = await openPanel(article, "Power by line");
  const read = await readPanel(panel);
  emit("conversation panel (SQL)", { read });
  check("conversation panel", !read.buttons.includes("Ask AI to explain"), "no Ask AI to explain");
  check("conversation panel", read.buttons.includes("Copy the SQL") && fact(read, "Rows") !== null, "copy and facts");
  const stats = await openPanel(article, "Daily average power");
  await page.waitForFunction(
    () => document.querySelector("[data-testid=request-summary]")?.textContent?.includes("line-2-meter"),
    null,
    { timeout: WAIT_MS },
  );
  const statsRead = await readPanel(stats);
  emit("conversation panel (statistics)", { read: statsRead, points: await stats.getByTestId("request-summary").getByRole("listitem").allTextContents() });
  check("conversation panel", !statsRead.buttons.includes("Ask AI to explain"), "no Ask AI to explain (statistics)");
  await context.close();
}

// 5. Dark theme, 1280.
{
  const { context, page } = await newPage({ context: { colorScheme: "dark" } });
  await openDashboard(page, "Line 2 power statistics");
  const panel = await openPanel(page, "Daily average power");
  const read = await readPanel(panel);
  emit("statistics panel (dark, 1280)", { read, metrics: await pageMetrics(page) });
  const metrics = await pageMetrics(page);
  check("dark theme", metrics.prefersDark, "the browser asked for dark");
  check("dark theme", read.background !== "rgba(0, 0, 0, 0)" && read.background !== "rgb(255, 255, 255)", "panel has a dark background");
  await context.close();
}

// 6. 390 px wide, light and dark: no horizontal scroll with the panel open.
for (const scheme of ["light", "dark"]) {
  const { context, page } = await newPage({ context: { viewport: { width: 390, height: 844 }, colorScheme: scheme } });
  await openDashboard(page, "Line 2 power statistics");
  // Before the panel is opened: what the page does without it.
  await page.getByRole("heading", { level: 3, name: "Daily average power" }).waitFor({ timeout: WAIT_MS });
  emit(`dashboard page, panel closed (${scheme}, 390)`, { metrics: await pageMetrics(page), overflowers: await overflowers(page) });
  const panel = await openPanel(page, "Daily average power");
  await panel.getByRole("button", { name: "Ask AI to explain" }).waitFor({ timeout: WAIT_MS });
  const read = await readPanel(panel);
  const metrics = await pageMetrics(page);
  const box = await panel.boundingBox();
  emit(`statistics panel (${scheme}, 390)`, { read, metrics, panelWidth: box?.width ?? null, overflowers: await overflowers(page) });
  check(`390 ${scheme}`, metrics.rootScrollWidth <= metrics.rootClientWidth && (await overflowers(page)).length === 0, "no horizontal scroll in the app");
  await page.goto(`${BASE}${CONVERSATION}`);
  const article = page.getByRole("article", { name: "TrustIoT's answer" });
  await article.waitFor({ timeout: WAIT_MS });
  await article.getByRole("heading", { level: 3, name: "Daily average power" }).waitFor({ timeout: WAIT_MS });
  emit(`conversation page, panel closed (${scheme}, 390)`, { metrics: await pageMetrics(page), overflowers: await overflowers(page) });
  await openPanel(article, "Daily average power");
  const conversationMetrics = await pageMetrics(page);
  emit(`conversation panel (${scheme}, 390)`, { metrics: conversationMetrics, overflowers: await overflowers(page) });
  check(
    `390 ${scheme} conversation`,
    conversationMetrics.rootScrollWidth <= conversationMetrics.rootClientWidth && (await overflowers(page)).length === 0,
    "no horizontal scroll in the app",
  );
  await context.close();
}

await browser.close();
emit("summary", { failures });
if (failures.length > 0) process.exit(1);
