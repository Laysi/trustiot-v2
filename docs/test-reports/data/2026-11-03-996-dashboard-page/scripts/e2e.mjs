// End-to-end: the SPA (production build, production headers) against the real dashboard API, a real
// Postgres and a real ClickHouse (services/assistant/tests/spa_e2e_996.rs). New readings go into
// ClickHouse between two "Refresh data" clicks; the page must show them, and show "Updating…"
// with the earlier content kept, dimmed, while the datasets run again.
import { chromium } from "/pw/playwright-core/index.mjs";
import { readFileSync, writeFileSync, existsSync } from "node:fs";

const ORIGIN = "http://host.docker.internal:28902";
const dir = "/e2e";
const read = (n) => readFileSync(`${dir}/${n}`, "utf8").trim();
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, fn, ms = 60000) => { const end = Date.now() + ms; for (;;) { const v = await fn(); if (v) return v; if (Date.now() > end) throw new Error(`timed out waiting for ${what}`); await wait(200); } };
const num = (x) => new Intl.NumberFormat("en", { maximumFractionDigits: 3 }).format(x);

const dashboard = read("dashboard");
const before = JSON.parse(read("expected-before.json"));
const log = { steps: [], violations: [], console: [], requests: [] };
const step = (name, data = {}) => { log.steps.push({ name, ...data }); console.log(name, JSON.stringify(data)); };

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, locale: "en-US", timezoneId: "UTC" });
const page = await context.newPage();
page.on("console", (m) => { if (["error", "warning"].includes(m.type())) log.console.push(`${m.type()}: ${m.text()}`); });
page.on("request", (r) => { if (r.url().includes("/dataset-runs") || r.url().includes("/sandbox")) log.requests.push({ method: r.method(), url: r.url().replace(ORIGIN, ""), body: r.postData() }); });
await page.addInitScript(`window.__PREVIEW = ${JSON.stringify({ real: true, role: "editor", theme: "light", language: "en" })}; window.__csp = []; document.addEventListener("securitypolicyviolation", (e) => window.__csp.push(e.effectiveDirective + " " + e.blockedURI));`);

// Hold every dataset run for 2.5 s once the first load is done, so the refresh can be observed.
let delay = 0;
await page.route("**/dataset-runs", async (route) => { if (delay) await wait(delay); await route.continue(); });

const valueOf = async (title) => (await page.getByRole("region", { name: title }).getByTestId("value").first().textContent()).trim();
await page.goto(`${ORIGIN}/dashboards/${dashboard}`);
await page.getByRole("heading", { level: 1, name: "Meter power" }).waitFor();
await until("the first load", async () => (await page.getByText("Loading data").count()) === 0 && (await valueOf("Peak power").catch(() => "")) !== "");
const shown1 = { peak: await valueOf("Peak power"), readings: await valueOf("Readings") };
step("first load", { shown: shown1, expected: { peak: num(before.all.peak_kw), readings: num(before.all.readings) } });
if (shown1.peak !== `${num(before.all.peak_kw)}kW` && !shown1.peak.startsWith(num(before.all.peak_kw))) throw new Error(`peak ${shown1.peak} != ${before.all.peak_kw}`);
if (!shown1.readings.startsWith(num(before.all.readings))) throw new Error(`readings ${shown1.readings} != ${before.all.readings}`);
const rowsText = async () => (await page.getByRole("region", { name: "Meters" }).getByRole("row").allTextContents()).map((t) => t.replace(/\s+/g, " ").trim());
const tableBefore = await rowsText();
for (const meter of before.meters) {
  const hit = tableBefore.some((row) => row.includes(meter.device_id) && row.includes(num(meter.readings)) && row.includes(num(meter.peak_kw)) && row.includes(num(meter.total_kw)));
  if (!hit) throw new Error(`no table row for ${JSON.stringify(meter)} in ${JSON.stringify(tableBefore)}`);
}
step("table before matches ClickHouse", { rows: tableBefore.length });
await page.screenshot({ path: "/out/e2e-before.png", fullPage: true });
const runsBefore = log.requests.filter((r) => r.url.endsWith("/dataset-runs")).length;

// New readings in ClickHouse.
writeFileSync(`${dir}/insert`, "1");
await until("the harness to insert", () => existsSync(`${dir}/inserted`));
const after = JSON.parse(read("expected-after.json"));
step("inserted", { expectedAfter: { peak: after.all.peak_kw, readings: after.all.readings } });

delay = 2500;
await page.getByRole("button", { name: "Refresh data" }).click();
await page.getByText("Updating…").waitFor({ timeout: 5000 });
const during = {
  updatingStatus: true,
  peakStillShown: await valueOf("Peak power"),
  dimmed: (await page.getByRole("region", { name: "Dashboard contents" }).getAttribute("class")).includes("opacity-60"),
  busy: await page.getByRole("region", { name: "Dashboard contents" }).getAttribute("aria-busy"),
  skeletons: await page.getByText("Loading data").count(),
  buttonDisabled: await page.getByRole("button", { name: "Refresh data" }).isDisabled(),
};
step("during the refresh", during);
if (during.peakStillShown !== shown1.peak || !during.dimmed || during.skeletons !== 0) throw new Error("the earlier content was not kept, dimmed");
await page.screenshot({ path: "/out/e2e-refreshing.png", fullPage: true });
await until("the refresh to finish", async () => (await page.getByText("Updating…").count()) === 0, 30000);
delay = 0;
const shown2 = { peak: await valueOf("Peak power"), readings: await valueOf("Readings") };
step("after the refresh", { shown: shown2, expected: { peak: num(after.all.peak_kw), readings: num(after.all.readings) } });
if (!shown2.peak.startsWith(num(after.all.peak_kw))) throw new Error(`peak after ${shown2.peak} != ${after.all.peak_kw}`);
if (!shown2.readings.startsWith(num(after.all.readings))) throw new Error(`readings after ${shown2.readings} != ${after.all.readings}`);
const tableAfter = await rowsText();
for (const meter of after.meters) {
  const hit = tableAfter.some((row) => row.includes(meter.device_id) && row.includes(num(meter.readings)) && row.includes(num(meter.peak_kw)) && row.includes(num(meter.total_kw)));
  if (!hit) throw new Error(`no table row after for ${JSON.stringify(meter)} in ${JSON.stringify(tableAfter)}`);
}
step("table after matches ClickHouse", {});
const runsAfter = log.requests.filter((r) => r.url.endsWith("/dataset-runs")).length;
step("dataset runs", { onOpening: runsBefore, afterRefresh: runsAfter });
await page.screenshot({ path: "/out/e2e-after.png", fullPage: true });
log.violations = await page.evaluate("window.__csp");
writeFileSync("/out/e2e-result.json", JSON.stringify({ ...log, shownBefore: shown1, shownAfter: shown2, tableBefore, tableAfter }, null, 1));
writeFileSync(`${dir}/done`, "1");
await browser.close();
console.log("E2E OK; CSP violations:", log.violations.length, "console:", log.console.length);
