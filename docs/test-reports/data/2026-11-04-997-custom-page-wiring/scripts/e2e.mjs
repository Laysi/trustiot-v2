// Browser check of the dashboard page's wiring to the custom page frame (Laysi/trustiot-v2#997):
// the real SPA build, served by the real `services/web` with its real CSP and sandbox headers,
// opens /dashboards/<id>. Only the API's JSON is answered by the script (fixtures.json, made by
// gen-fixtures.ts from the component tests' in-memory server); every other request goes to
// `services/web`. Runs in the Playwright container; environment:
//   E2E_BASE_URL  the address `services/web` is reachable at from the browser
//   E2E_FIXTURES  fixtures.json     E2E_OUT  the output directory (result.json and screenshots)
import { readFileSync, writeFileSync } from "node:fs";
import { chromium } from "playwright-core";

const BASE = process.env.E2E_BASE_URL;
const OUT = process.env.E2E_OUT ?? "/out";
const fixtures = JSON.parse(readFileSync(process.env.E2E_FIXTURES ?? "/fixtures.json", "utf8"));
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const until = async (what, fn, ms = 60000) => {
  const end = Date.now() + ms;
  for (;;) {
    const value = await fn();
    if (value) return value;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await wait(100);
  }
};
const json = (body, status = 200) => ({ status, contentType: "application/json", body: JSON.stringify(body) });

const result = { checks: [], violations: [], console: [], apiRequests: [], sandboxResponses: [] };
const check = (name, ok, detail = {}) => {
  result.checks.push({ name, ok, ...detail });
  console.log(ok ? "PASS" : "FAIL", name, JSON.stringify(detail));
  if (!ok) process.exitCode = 1;
};

const browser = await chromium.launch({ args: ["--site-per-process"] });
const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, locale: "en-US", timezoneId: "UTC" });
const page = await context.newPage();
page.on("console", (m) => {
  if (["error", "warning"].includes(m.type())) result.console.push(`${m.type()}: ${m.text()}`);
});
page.on("pageerror", (e) => result.console.push(`pageerror: ${e.message}`));
page.on("response", (r) => {
  const url = new URL(r.url());
  if (url.pathname.startsWith("/sandbox/")) {
    const h = r.headers();
    result.sandboxResponses.push({
      path: url.pathname,
      status: r.status(),
      csp: h["content-security-policy"] ?? null,
      xFrameOptions: h["x-frame-options"] ?? null,
      corp: h["cross-origin-resource-policy"] ?? null,
    });
  }
});
await page.addInitScript(() => {
  window.__csp = [];
  document.addEventListener("securitypolicyviolation", (e) => window.__csp.push(`${e.disposition} ${e.effectiveDirective} ${e.blockedURI}`));
});

// The API: the fixtures' answers.
await page.route("**/api/**", async (route) => {
  const request = route.request();
  const url = new URL(request.url());
  const path = url.pathname;
  result.apiRequests.push(`${request.method()} ${path}`);
  if (path === "/api/identity/v1/sessions/current") return route.fulfill(json(fixtures.session));
  if (path === "/api/identity/v1/preferences") return route.fulfill(json(fixtures.preferences));
  const match = /^\/api\/assistant\/v1\/dashboards\/([^/]+)(\/dataset-runs)?$/.exec(path);
  if (match) {
    const dashboard = Object.values(fixtures.dashboards).find((d) => d.id === match[1]);
    if (dashboard && !match[2] && request.method() === "GET") return route.fulfill(json(dashboard.view));
    if (dashboard && match[2] && request.method() === "POST") {
      const run = dashboard.runs[request.postDataJSON().dataset];
      if (run) return route.fulfill(json(run));
    }
  }
  return route.fulfill(json({ code: "not_found", message: "unexpected request in the check" }, 404));
});

const state = (title) =>
  page.evaluate((name) => {
    const frame = document.querySelector(`iframe[title="${name}"]`);
    if (frame === null) return { present: false };
    return { present: true, shown: frame.isConnected && frame.style.opacity === "1" && !frame.inert, opacity: frame.style.opacity, inert: frame.inert, ariaHidden: frame.getAttribute("aria-hidden") };
  }, title);
const frameValue = async (title) => {
  const handle = await page.locator(`iframe[title="${title}"]`).elementHandle();
  const content = await handle?.contentFrame();
  return content ? content.evaluate(() => document.getElementById("v")?.textContent ?? null).catch(() => null) : null;
};

// 1. The clean page: it takes the item's place, in the sandbox frame, and is shown after a clean comparison.
const clean = fixtures.dashboards.clean;
const response = await page.goto(`${BASE}/dashboards/${clean.id}`);
const spaHeaders = response.headers();
result.spaCsp = spaHeaders["content-security-policy"] ?? null;
await page.getByRole("heading", { level: 1, name: "Plant power" }).waitFor();
const frameLocator = page.locator('iframe[title="Plant power"]');
await frameLocator.waitFor({ state: "attached", timeout: 60000 });
const attributes = await frameLocator.evaluate((f) => ({ sandbox: f.getAttribute("sandbox"), src: f.getAttribute("src"), title: f.getAttribute("title") }));
check("the item is an iframe with the sandbox attribute", attributes.sandbox !== null, attributes);
check("its src is the sandbox document of runtime 1", /^\/sandbox\/v1\/sandbox\.html/.test(attributes.src ?? ""), { src: attributes.src });
check("the sandbox grants no same-origin", !(attributes.sandbox ?? "").split(/\s+/).includes("allow-same-origin"), { sandbox: attributes.sandbox });
const hiddenFirst = await state("Plant power");
await until("the frame to be shown after a clean comparison", async () => (await state("Plant power")).shown);
const shown = await state("Plant power");
check("the frame is shown", shown.shown, shown);
const painted = await until("the page to have drawn its value", () => frameValue("Plant power"));
check("the page drew the first power cell, formatted", painted === "12.35", { painted });
const labels = await page.evaluate(() => {
  const card = document.querySelector('[data-component="custom_page"]');
  return { present: card !== null, label: card?.textContent?.includes("Custom chart") ?? false, heading: card?.querySelector("h3")?.textContent ?? null, cards: document.querySelectorAll("[data-custom-page]").length, kind: [...document.querySelectorAll("[data-component]")].map((e) => e.getAttribute("data-component")), cardOfFrame: !!document.querySelector('[data-custom-page] iframe') };
});
check("the frame's own card: heading, label, the one card of the item", labels.present && labels.label && labels.heading === "Plant power" && labels.cards === 1 && labels.cardOfFrame, labels);
check("the item keeps its place after the KPI", JSON.stringify(labels.kind) === JSON.stringify(["kpi", "custom_page"]), { kinds: labels.kind });
check("the placeholder message is gone", (await page.getByText("This item can't be shown.").count()) === 0);
const sandboxDoc = result.sandboxResponses.find((r) => r.path === "/sandbox/v1/sandbox.html");
check("the sandbox document came from services/web with its sandbox headers", !!sandboxDoc && sandboxDoc.status === 200 && sandboxDoc.csp !== null && sandboxDoc.xFrameOptions === null, sandboxDoc ?? {});
check("the SPA's CSP frames only the sandbox document", /frame-src [^;]*\/sandbox\/v1\/sandbox\.html/.test(result.spaCsp ?? ""), { csp: result.spaCsp });
result.cleanSequence = { hiddenFirst, shown };
await page.screenshot({ path: `${OUT}/clean-shown.png`, fullPage: true });
// The refresh button of the page header reads the datasets again; the frame stays shown with the same values.
const runsBefore = result.apiRequests.filter((r) => r.endsWith("/dataset-runs")).length;
await page.getByRole("button", { name: "Refresh data" }).click();
await until("the datasets to run again", () => result.apiRequests.filter((r) => r.endsWith("/dataset-runs")).length > runsBefore);
await until("the frame to be shown again after the refresh", async () => (await state("Plant power")).shown);
check("after a refresh the frame is shown again with the same value", (await frameValue("Plant power")) === "12.35");
result.cleanViolations = await page.evaluate(() => window.__csp);
check("no CSP violation on the clean page", result.cleanViolations.length === 0, { violations: result.cleanViolations });

// 2. A page that writes a value the data does not have: never shown with it.
const tampering = fixtures.dashboards.tampering;
await page.goto(`${BASE}/dashboards/${tampering.id}`);
await page.getByRole("heading", { level: 1, name: "Plant power (tampered)" }).waitFor();
const samples = [];
const started = Date.now();
await until("the frame to exist", async () => (await state("Plant power (tampered)")).present);
let sawShown = false;
let lastNotice = null;
while (Date.now() - started < 20000) {
  const s = await state("Plant power (tampered)");
  const value = await frameValue("Plant power (tampered)");
  const notice = await page.getByTestId("custom-page-notice").textContent().catch(() => null);
  if (notice) lastNotice = notice;
  samples.push({ t: Date.now() - started, shown: s.present && s.shown, present: s.present, value });
  if (s.shown) sawShown = true;
  await wait(50);
}
const wrongWhileShown = samples.filter((s) => s.shown && s.value === "999.99");
check("the page was shown at first with the right value (the tampering comes two seconds later)", sawShown);
check("the frame is never shown while the page paints the value the data does not have", wrongWhileShown.length === 0, { wrongWhileShown: wrongWhileShown.length, samples: samples.length });
check("the frame ends hidden or closed, with a notice saying why", !samples.at(-1).shown && lastNotice !== null, { last: samples.at(-1), notice: lastNotice });
result.tamperingSamples = { count: samples.length, transitions: samples.filter((s, i) => i === 0 || s.shown !== samples[i - 1].shown || s.present !== samples[i - 1].present || s.value !== samples[i - 1].value) };
await page.screenshot({ path: `${OUT}/tampering-hidden.png`, fullPage: true });
result.tamperingViolations = await page.evaluate(() => window.__csp);

result.console = result.console.filter((line) => !/Failed to load resource/.test(line));
check("no console errors or page errors", result.console.length === 0, { console: result.console });
writeFileSync(`${OUT}/result.json`, `${JSON.stringify(result, null, 1)}\n`);
await browser.close();
console.log(process.exitCode ? "E2E FAILED" : "E2E OK");
