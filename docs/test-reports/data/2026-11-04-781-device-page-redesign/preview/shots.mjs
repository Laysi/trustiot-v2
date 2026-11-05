import { writeFileSync } from "node:fs";
import { chromium } from "/pw/playwright-core/index.mjs";

const ORIGIN = process.env.ORIGIN ?? "http://127.0.0.1:28911";
const OUT = process.env.OUT ?? "/out";
const ONLY = process.env.ONLY ? process.env.ONLY.split(",") : null;
const VARIANTS = (process.env.VARIANTS ?? "desktop-light,desktop-dark,phone-light,phone-dark").split(",");
const SIZES = { desktop: { width: 1280, height: 900 }, phone: { width: 390, height: 844 } };
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const json = (status, body) =>
  `new Response(JSON.stringify(${JSON.stringify(body)}), {status:${status}, headers:{"content-type":"application/json"}})`;
const evalFake = (page, code) => page.evaluate(`(() => { const {server, ids, holds} = window.__fake; ${code} })()`);
const settle = async (page, ms = 1800) => {
  await page.waitForLoadState("networkidle").catch(() => {});
  await wait(ms);
};

// Drags a range on the chart (a mouse selects), then reads what the page shows.
async function dragZoom(page, from, to) {
  const box = await page.locator('[data-testid="chart-root"]').boundingBox();
  const y = box.y + box.height * 0.45;
  await page.mouse.move(box.x + box.width * from, y);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width * ((from + to) / 2), y, { steps: 6 });
  await page.mouse.move(box.x + box.width * to, y, { steps: 6 });
  await page.mouse.up();
  await wait(900);
}

// Each state: name, path (may use {key}), before(page) after the app is up, after(page) after load.
const STATES = [
  { name: "normal", path: "/devices/{normal}" },
  {
    name: "hover",
    path: "/devices/{normal}",
    after: async (p, form, results) => {
      if (form === "desktop") {
        await dragZoom(p, 0.4, 0.62);
        const zoomed = await p.getByText(/^已放大到/).count();
        const label = await p.getByText("平均（放大範圍）").count();
        results.checks = { zoomed, label };
        const box = await p.locator('[data-testid="chart-root"]').boundingBox();
        await p.mouse.move(box.x + box.width * 0.5, box.y + box.height * 0.4);
        await wait(700);
        results.checks.tooltip = await p.locator('[data-testid="chart-tooltip"]').count();
      } else {
        const box = await p.locator('[data-testid="chart-root"]').boundingBox();
        await p.touchscreen.tap(box.x + box.width * 0.5, box.y + box.height * 0.4);
        await wait(700);
        results.checks = { tooltipAfterTap: await p.locator('[data-testid="chart-tooltip"]').count() };
        if (results.checks.tooltipAfterTap === 0) {
          await p.mouse.click(box.x + box.width * 0.5, box.y + box.height * 0.4);
          await wait(700);
          results.checks.tooltipAfterClick = await p.locator('[data-testid="chart-tooltip"]').count();
        }
      }
    },
  },
  {
    name: "set-range",
    path: "/devices/{normal}",
    only: "desktop",
    after: async (p, form, results) => {
      await dragZoom(p, 0.4, 0.62);
      await p.getByRole("button", { name: "把這段設為範圍" }).click();
      await settle(p, 1500);
      results.checks = {
        url: await p.evaluate("location.search"),
        chart: await p.locator('[data-testid="chart-root"]').count(),
      };
    },
  },
  { name: "bool", path: "/devices/{normal}?point=breaker" },
  { name: "text", path: "/devices/{normal}?point=status" },
  {
    name: "loading",
    path: "/devices/{normal}",
    before: (p) => evalFake(p, `holds.add("device")`),
    shotMs: 700,
  },
  {
    name: "loading-history",
    path: "/devices/{normal}",
    before: (p) => evalFake(p, `holds.add("series")`),
    shotMs: 900,
  },
  { name: "stale", path: "/devices/{stale}" },
  { name: "never", path: "/devices/{never}" },
  {
    name: "never-older",
    path: "/devices/{never}",
    after: async (p) => {
      await p.getByRole("button", { name: "查看更早的歷史" }).click();
      await settle(p, 1500);
    },
  },
  {
    name: "never-model-failed",
    path: "/devices/{never}",
    before: (p) =>
      evalFake(p, `server.failNext("model", ${json(503, { code: "unavailable", message_key: "error.unavailable" })})`),
  },
  {
    name: "custom-edit",
    path: "/devices/{normal}?from=2026-11-01T06:00:00.000Z&to=2026-11-01T08:00:00.000Z",
    only: "phone",
    after: async (p, form, results) => {
      await p.getByRole("button", { name: "修改範圍" }).click();
      await wait(400);
      results.checks = { form: await p.getByLabel("開始").count() };
    },
  },
  { name: "nopoints", path: "/devices/{nopoints}" },
  { name: "outside", path: "/devices/{normal}?from=2025-01-01T00:00:00.000Z&to=2025-01-02T00:00:00.000Z" },
  {
    name: "truncated",
    path: "/devices/{truncated}?range=90d",
    before: (p) => evalFake(p, `server.aggregatedPeriodMs = 60 * 86400000`),
  },
  { name: "rangeempty", path: "/devices/{stale}?range=1h" },
  {
    name: "custom",
    path: "/devices/{normal}",
    after: async (p, form) => {
      if (form === "desktop") await p.getByRole("button", { name: "自訂" }).click();
      else await p.getByLabel("時間範圍").selectOption("custom");
      await wait(400);
      await p.getByLabel("結束").fill("2026-11-01T06:00");
      await p.getByLabel("開始").fill("2026-11-01T08:00");
      await p.getByRole("button", { name: "顯示", exact: true }).click();
      await wait(500);
    },
  },
  {
    name: "error",
    path: "/devices/{normal}",
    before: (p) =>
      evalFake(p, `server.failNext("series", ${json(503, { code: "unavailable", message_key: "error.unavailable" })})`),
  },
  {
    name: "model-failed",
    path: "/devices/{normal}",
    before: (p) =>
      evalFake(p, `server.failNext("model", ${json(503, { code: "unavailable", message_key: "error.unavailable" })})`),
  },
  {
    name: "pageerror",
    path: "/devices/{normal}",
    before: (p) =>
      evalFake(p, `server.failNext("device", ${json(503, { code: "unavailable", message_key: "error.unavailable" })})`),
  },
  { name: "notfound", path: "/devices/00000000-0000-4000-8000-00000000ffff" },
  { name: "long", path: "/devices/{long}?point=thd_voltage_percent_phase_a_long_key_name" },
  {
    name: "long-more",
    path: "/devices/{long}",
    after: async (p) => {
      await p.getByRole("button", { name: /顯示其他/ }).click();
      await wait(300);
    },
  },
  {
    name: "table",
    path: "/devices/{normal}?range=1h",
    after: async (p) => {
      await p.getByRole("button", { name: /以表格顯示/ }).click();
      await wait(400);
    },
  },
];

const browser = await chromium.launch();
const results = [];
for (const state of STATES) {
  if (ONLY && !ONLY.includes(state.name)) continue;
  for (const variant of VARIANTS) {
    const [form, theme] = variant.split("-");
    if (state.only && state.only !== form) continue;
    const context = await browser.newContext({
      viewport: SIZES[form],
      deviceScaleFactor: 1,
      locale: "zh-TW",
      timezoneId: "Asia/Taipei",
      hasTouch: form === "phone",
      isMobile: false,
    });
    const page = await context.newPage();
    const log = { console: [], csp: [], pageErrors: [], checks: undefined };
    page.on("console", (m) => {
      if (["error", "warning"].includes(m.type())) log.console.push(`${m.type()}: ${m.text()}`);
    });
    page.on("pageerror", (e) => log.pageErrors.push(String(e)));
    await page.addInitScript(`
      window.__PREVIEW = ${JSON.stringify({ theme, language: "zh-TW" })};
      window.__csp = [];
      document.addEventListener("securitypolicyviolation", (e) => window.__csp.push(e.effectiveDirective + " " + e.blockedURI));
    `);
    await page.goto(`${ORIGIN}/`, { waitUntil: "domcontentloaded" });
    await page.waitForFunction("window.__fake !== undefined");
    const ids = await page.evaluate("window.__fake.ids");
    try {
      await state.before?.(page);
      const path = state.path.replace(/\{(\w+)\}/g, (_, k) => ids[k] ?? k);
      // Client-side navigation keeps the in-memory servers (a reload would rebuild them).
      await page.evaluate((p) => {
        window.history.pushState({}, "", p);
        window.dispatchEvent(new PopStateEvent("popstate"));
      }, path);
      await settle(page, state.shotMs ?? 1800);
      await state.after?.(page, form, log);
      await wait(500);
    } catch (e) {
      log.pageErrors.push(`STEP FAILED: ${String(e).slice(0, 300)}`);
    }
    log.csp = await page.evaluate("window.__csp");
    log.overflowPx = await page.evaluate(
      "document.documentElement.scrollWidth - document.documentElement.clientWidth",
    );
    const file = `${OUT}/781-${state.name}-${form}-${theme}.png`;
    await page.screenshot({ path: file, fullPage: true });
    results.push({ state: state.name, variant, file: file.split("/").pop(), ...log });
    await context.close();
  }
}
await browser.close();
writeFileSync(`${OUT}/results.json`, JSON.stringify(results, null, 1));
const bad = results.filter((r) => r.csp.length || r.pageErrors.length || r.console.length || r.overflowPx > 0);
console.log(`${results.length} screenshots; with console/CSP output or overflow: ${bad.length}`);
for (const b of bad)
  console.log(
    b.state,
    b.variant,
    JSON.stringify({ csp: b.csp, pageErrors: b.pageErrors, console: b.console, overflowPx: b.overflowPx }).slice(
      0,
      500,
    ),
  );
const checked = results.filter((r) => r.checks);
for (const c of checked) console.log("checks", c.state, c.variant, JSON.stringify(c.checks));

// The run fails when any screenshot has console, CSP or page errors or overflow, or when a required
// interaction check fails. A phone tap showing the tooltip is not required: synthetic touch events do
// not reach the chart (the report says so).
const required = (state, form) => results.filter((r) => r.state === state && r.variant.startsWith(`${form}-`));
const interactionFailures = [
  ...required("hover", "desktop").filter(
    (r) => !(r.checks?.zoomed === 1 && r.checks?.label === 1 && r.checks?.tooltip === 1),
  ),
  ...required("set-range", "desktop").filter((r) => !(r.checks?.chart === 1 && /[?&]from=/.test(r.checks?.url ?? ""))),
  ...required("custom-edit", "phone").filter((r) => r.checks?.form !== 1),
];
for (const failure of interactionFailures) console.log("interaction check failed", failure.state, failure.variant);
if (bad.length > 0 || interactionFailures.length > 0) process.exitCode = 1;
