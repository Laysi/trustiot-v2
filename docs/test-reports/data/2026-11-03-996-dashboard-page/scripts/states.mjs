import { chromium } from "/pw/playwright-core/index.mjs";
import { writeFileSync } from "node:fs";

const ORIGIN = process.env.ORIGIN ?? "http://host.docker.internal:28901";
const ONLY = process.env.ONLY ? process.env.ONLY.split(",") : null;
const VARIANTS = (process.env.VARIANTS ?? "desktop-light,desktop-dark,phone-light,phone-dark").split(",");
const SIZES = { desktop: { width: 1280, height: 900 }, phone: { width: 390, height: 844 } };
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const json = (status, body) => `new Response(JSON.stringify(${JSON.stringify(body)}), {status:${status}, headers:{"content-type":"application/json"}})`;

const evalFake = (page, code) => page.evaluate(`(() => { const {server, ids} = window.__fake; ${code} })()`);
const settle = async (page, ms = 1600) => { await page.waitForLoadState("networkidle").catch(() => {}); await wait(ms); };
const openMenu = async (page, name) => { await page.getByRole("button", { name, exact: true }).focus(); await page.keyboard.press("Enter"); await wait(400); };

// Each state: name, path (may use {id:key}), config, before(page) hook run before navigation, after(page) after load.
const STATES = [
  { name: "list", path: "/dashboards" },
  { name: "list-archived-tab", path: "/dashboards?tab=archived" },
  { name: "list-viewer", path: "/dashboards", config: { role: "viewer" } },
  { name: "list-empty", path: "/dashboards", config: { empty: true } },
  { name: "list-loading", path: "/dashboards", before: (p) => evalFake(p, `server.hold(["list"])`), shotMs: 600 },
  { name: "list-error", path: "/dashboards", before: (p) => evalFake(p, `server.failNext("list", ${json(503, { code: "unavailable", message_key: "error.unavailable" })})`) },
  { name: "list-search-none", path: "/dashboards", after: async (p) => { await p.getByRole("searchbox").fill("鍋爐"); await wait(300); } },
  { name: "list-archived-action", path: "/dashboards", after: async (p) => { await p.getByRole("button", { name: /^封存「空壓機/ }).click(); await wait(800); } },
  { name: "dashboard", path: "/dashboards/{cooling}" },
  { name: "dashboard-skeleton", path: "/dashboards/{cooling}", before: (p) => evalFake(p, `server.hold(["dashboard"])`), shotMs: 600 },
  { name: "dashboard-datasets-loading", path: "/dashboards/{cooling}", before: (p) => evalFake(p, `server.hold(["run"])`), shotMs: 800 },
  { name: "dashboard-refreshing", path: "/dashboards/{cooling}", after: async (p) => { await evalFake(p, `server.hold(["run"])`); await p.getByRole("button", { name: "更新資料" }).click(); await wait(900); } },
  { name: "dashboard-dataset-failed", path: "/dashboards/{cooling}", before: (p) => evalFake(p, `server.failNext("run:daily", ${json(504, { code: "timeout", message_key: "dashboard.dataset_timeout" })})`) },
  { name: "dashboard-rate-limited", path: "/dashboards/{cooling}", before: (p) => evalFake(p, `server.failNext("run:daily", ${json(429, { code: "rate_limited", message_key: "dashboard.runs_limited", details: { retry_after: 3 } })})`) },
  { name: "dashboard-viewer", path: "/dashboards/{cooling}", config: { role: "viewer" } },
  { name: "dashboard-old-version", path: "/dashboards/{cooling}?version=2" },
  { name: "dashboard-versions", path: "/dashboards/{cooling}", after: async (p) => { await p.getByRole("button", { name: "版本紀錄" }).click(); await wait(900); } },
  { name: "dashboard-restore-dialog", path: "/dashboards/{cooling}?version=2", after: async (p) => { await p.getByRole("button", { name: "回到第 2 版" }).click(); await wait(600); } },
  { name: "dashboard-archived", path: "/dashboards/{weekly}", after: async (p) => { await p.getByRole("button", { name: "封存" }).click(); await wait(1200); } },
  { name: "dashboard-custom-page", path: "/dashboards/{custom}" },
  { name: "dashboard-source-panel", path: "/dashboards/{weekly}", after: async (p) => { await p.getByRole("button", { name: /來源/ }).first().click(); await wait(800); } },
  { name: "dashboard-not-found", path: "/dashboards/00000000-0000-4000-8000-00000000ffff" },
  { name: "menu-open", path: "/dashboards/{cooling}", after: (p) => openMenu(p, /^選單$/) },
  { name: "tenant-menu-open", path: "/dashboards", after: (p) => openMenu(p, /^切換組織/) },
  { name: "account-menu-open", path: "/dashboards", only: "desktop", after: (p) => openMenu(p, /^帳號$/) },
  { name: "home-header", path: "/" },
];

const browser = await chromium.launch();
const results = [];
for (const state of STATES) {
  if (ONLY && !ONLY.includes(state.name)) continue;
  for (const variant of VARIANTS) {
    const [form, theme] = variant.split("-");
    if (state.only && state.only !== form) continue;
    const context = await browser.newContext({ viewport: SIZES[form], deviceScaleFactor: 1, locale: "zh-TW", timezoneId: "Asia/Taipei", hasTouch: form === "phone", isMobile: false });
    const page = await context.newPage();
    const log = { console: [], csp: [], pageErrors: [], requests: [] };
    page.on("console", (m) => { if (["error", "warning"].includes(m.type())) log.console.push(`${m.type()}: ${m.text()}`); });
    page.on("pageerror", (e) => log.pageErrors.push(String(e)));
    page.on("request", (r) => { if (r.url().includes("/sandbox")) log.requests.push(r.url()); });
    await page.addInitScript(`
      window.__PREVIEW = ${JSON.stringify({ role: "editor", theme, language: "zh-TW", ...(state.config ?? {}) })};
      window.__csp = [];
      document.addEventListener("securitypolicyviolation", (e) => window.__csp.push(e.effectiveDirective + " " + e.blockedURI));
    `);
    // The harness's server is created by the page; hooks run once it exists.
    await page.goto(`${ORIGIN}/`, { waitUntil: "domcontentloaded" });
    await page.waitForFunction("window.__fake !== undefined");
    const ids = await page.evaluate("window.__fake.ids");
    try {
    await state.before?.(page);
    const path = state.path.replace(/\{(\w+)\}/g, (_, k) => ids[k] ?? k);
    // Client-side navigation keeps the in-memory servers (a reload would rebuild them).
    await page.evaluate((p) => { window.history.pushState({}, "", p); window.dispatchEvent(new PopStateEvent("popstate")); }, path);
    await settle(page, state.shotMs ?? 1600);
    await state.after?.(page);
    await wait(500);
    } catch (e) { log.pageErrors.push("STEP FAILED: " + String(e).slice(0, 200)); }
    log.csp = await page.evaluate("window.__csp");
    log.overflowPx = await page.evaluate("document.documentElement.scrollWidth - document.documentElement.clientWidth - (window.innerWidth - document.documentElement.clientWidth)");
    const file = `/out/996-${state.name}-${form}-${theme}.png`;
    await page.screenshot({ path: file, fullPage: !["menu-open", "tenant-menu-open", "account-menu-open"].includes(state.name) });
    results.push({ state: state.name, variant, file, ...log });
    await context.close();
  }
}
await browser.close();
writeFileSync("/out/results.json", JSON.stringify(results, null, 1));
const bad = results.filter((r) => r.csp.length || r.pageErrors.length || r.console.length || r.overflowPx > 0);
console.log(`${results.length} screenshots; with console/CSP output: ${bad.length}`);
for (const b of bad) console.log(b.state, b.variant, JSON.stringify({ csp: b.csp, pageErrors: b.pageErrors, console: b.console, overflowPx: b.overflowPx }).slice(0, 400));
