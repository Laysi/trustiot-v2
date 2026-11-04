// Interaction flow under the production CSP (CSP violations and console output are collected), and
// contrast of the shell's and the pages' own colors, read from the computed styles in the browser.
import { chromium } from "/pw/playwright-core/index.mjs";
import { writeFileSync } from "node:fs";
const ORIGIN = process.env.ORIGIN ?? "http://host.docker.internal:28901";
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const out = { flows: [], contrast: [] };

async function newPage(browser, theme, size, config = {}) {
  const context = await browser.newContext({ viewport: size, locale: "zh-TW", timezoneId: "Asia/Taipei" });
  const page = await context.newPage();
  page.log = { console: [], pageErrors: [], sandbox: [] };
  page.on("console", (m) => { if (["error", "warning"].includes(m.type())) page.log.console.push(`${m.type()}: ${m.text()}`); });
  page.on("pageerror", (e) => page.log.pageErrors.push(String(e)));
  page.on("request", (r) => { if (r.url().includes("/sandbox")) page.log.sandbox.push(r.url()); });
  await page.addInitScript(`window.__PREVIEW = ${JSON.stringify({ role: "editor", theme, language: "zh-TW", ...config })}; window.__csp = []; document.addEventListener("securitypolicyviolation", (e) => window.__csp.push(e.effectiveDirective + " " + e.blockedURI + " @" + e.sourceFile));`);
  await page.goto(`${ORIGIN}/`, { waitUntil: "domcontentloaded" });
  await page.waitForFunction("window.__fake !== undefined");
  return page;
}
const go = async (page, path) => { await page.evaluate((p) => { window.history.pushState({}, "", p); window.dispatchEvent(new PopStateEvent("popstate")); }, path); await wait(1500); };
const finish = async (page, name, steps) => { out.flows.push({ name, steps, csp: await page.evaluate("window.__csp"), ...page.log }); };

const browser = await chromium.launch();
for (const theme of ["light", "dark"]) {
  // Flow 1: the pages and every panel, with the keyboard and the mouse.
  const page = await newPage(browser, theme, { width: 1280, height: 900 });
  const ids = await page.evaluate("window.__fake.ids");
  const steps = [];
  await go(page, "/dashboards"); steps.push("opened the list");
  await page.getByRole("button", { name: "已封存" }).click(); await wait(500); steps.push("archived tab");
  await page.getByRole("button", { name: "使用中" }).click(); await wait(500);
  await page.getByRole("searchbox").fill("冷卻"); await wait(300); steps.push("searched");
  await page.getByRole("link", { name: "冷卻水塔監控", exact: true }).click(); await wait(2000); steps.push("opened a dashboard from the list");
  await page.getByRole("button", { name: "版本紀錄" }).click(); await wait(600); steps.push("opened the version history");
  await page.getByRole("link", { name: "檢視第 2 版" }).click(); await wait(2000); steps.push("viewed version 2 (deep link ?version=2)");
  await page.getByRole("button", { name: "回到第 2 版" }).first().click(); await wait(600); steps.push("opened the restore dialog");
  await page.keyboard.press("Escape"); await wait(400);
  await page.getByRole("button", { name: /來源/ }).first().click(); await wait(800); steps.push("opened a source panel");
  await page.keyboard.press("Escape"); await wait(300);
  await page.getByRole("button", { name: "更新資料" }).click(); await wait(1500); steps.push("refreshed");
  const menu = page.getByRole("button", { name: "選單", exact: true });
  await menu.focus(); await page.keyboard.press("Enter"); await wait(400); steps.push("opened the menu");
  await page.keyboard.press("ArrowDown"); await page.keyboard.press("Escape"); await wait(300);
  const focusBack = await page.evaluate("document.activeElement?.textContent?.trim() ?? ''"); steps.push(`Escape closed the menu; focus is on: ${focusBack}`);
  await page.getByRole("button", { name: /^切換組織/ }).focus(); await page.keyboard.press("Enter"); await wait(400); await page.keyboard.press("Escape"); await wait(300); steps.push("opened and closed the organization menu");
  await page.getByRole("button", { name: "帳號", exact: true }).focus(); await page.keyboard.press("Enter"); await wait(400); await page.keyboard.press("Escape"); steps.push("opened and closed the account menu");
  await go(page, `/dashboards/${ids.custom}`); steps.push("opened a dashboard with a custom page item");
  await finish(page, `desktop-${theme}`, steps);
  await page.context().close();

  // Flow 2: the phone.
  const phone = await newPage(browser, theme, { width: 390, height: 844 });
  const psteps = [];
  await go(phone, "/dashboards");
  const listOverflow = await phone.evaluate("document.documentElement.scrollWidth - document.documentElement.clientWidth"); psteps.push(`horizontal overflow of the list page: ${listOverflow}px`);
  await go(phone, `/dashboards/${(await phone.evaluate("window.__fake.ids")).cooling}`); psteps.push("opened a dashboard");
  await phone.getByRole("button", { name: "選單", exact: true }).click(); await wait(500); psteps.push("opened the phone menu");
  await phone.keyboard.press("Escape"); await wait(300);
  const overflow = await phone.evaluate("document.documentElement.scrollWidth - document.documentElement.clientWidth"); psteps.push(`horizontal overflow of the page: ${overflow}px`);
  const headerBox = await phone.evaluate("(() => { const h = document.querySelector('header'); const r = h.getBoundingClientRect(); const kids=[...h.querySelectorAll('a,button')].map(e=>e.getBoundingClientRect()); return {height:r.height, tops:[...new Set(kids.map(k=>Math.round(k.top)))], right: Math.max(...kids.map(k=>k.right)), left: Math.min(...kids.map(k=>k.left))}; })()"); psteps.push(`header: ${JSON.stringify(headerBox)}`);
  await finish(phone, `phone-${theme}`, psteps);
  await phone.context().close();

  // Contrast of colors, from the computed styles.
  const c = await newPage(browser, theme, { width: 1280, height: 900 });
  const cids = await c.evaluate("window.__fake.ids");
  await go(c, `/dashboards/${cids.cooling}?version=2`);
  await c.getByRole("button", { name: "選單", exact: true }).click(); await wait(500);
  const measure = await c.evaluate(`(() => {
    const canvas = document.createElement("canvas"); canvas.width = canvas.height = 1; const ctx = canvas.getContext("2d", { willReadFrequently: true });
    const rgba = (css) => { ctx.clearRect(0,0,1,1); ctx.fillStyle = "#000"; ctx.fillStyle = css; ctx.fillRect(0,0,1,1); const d = ctx.getImageData(0,0,1,1).data; return [d[0], d[1], d[2], d[3] / 255]; };
    const over = (top, under) => { const a = top[3]; return [0,1,2].map(i => top[i] * a + under[i] * (1 - a)).concat([1]); };
    const bgOf = (el) => { const stack = []; for (let e = el; e; e = e.parentElement) { const b = rgba(getComputedStyle(e).backgroundColor); if (b[3] > 0) { stack.push(b); if (b[3] === 1) break; } } let acc = rgba(getComputedStyle(document.body).backgroundColor); if (acc[3] === 0) acc = [255,255,255,1]; for (const b of stack.reverse()) acc = over(b, acc); return acc; };
    const lin = (v) => { v /= 255; return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; };
    const lum = (c) => 0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2]);
    const ratio = (a, b) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };
    const find = (sel, text) => [...document.querySelectorAll(sel)].find((e) => (e.textContent ?? "").trim().includes(text));
    const results = [];
    const textPair = (name, el, need = 4.5) => { if (!el) { results.push({ name, error: "element not found" }); return; } const bg = bgOf(el); const fg = over(rgba(getComputedStyle(el).color), bg); results.push({ name, ratio: +ratio(fg, bg).toFixed(2), need }); };
    const colorPair = (name, color, el, need = 3) => { if (!el) { results.push({ name, error: "element not found" }); return; } const bg = bgOf(el); results.push({ name, ratio: +ratio(over(rgba(color), bg), bg).toFixed(2), need }); };
    const menu = document.querySelector('[role="menu"]');
    const current = menu?.querySelector('[aria-current="page"]');
    textPair("header: menu button text on the header", find("header button", "選單"));
    textPair("header: organization name on the header", find("header button", "東區示範廠"));
    textPair("header: back to the conversation on the header", find("header a", "回到對話"));
    textPair("menu: current item text on its background", current);
    colorPair("menu: current item icon (primary) on its background", getComputedStyle(current.querySelector("svg")).color, current);
    colorPair("menu: current item bar (primary) on its background", getComputedStyle(current, "::before").backgroundColor, current);
    textPair("menu: other item text on the menu", [...menu.querySelectorAll('[role="menuitem"]')].find((e) => !e.hasAttribute("aria-current")));
    colorPair("menu: other item icon on the menu", getComputedStyle([...menu.querySelectorAll('[role="menuitem"]')].find((e) => !e.hasAttribute("aria-current")).querySelector("svg")).color, menu);
    textPair("breadcrumb link on the page", find("nav a", "已存的儀表板"));
    textPair("tag: published/old version (warning) on its soft background", find("main span", "第 2 版（最新是第 4 版）"));
    textPair("meta line (data time) on the page", find("main p", "資料時間"));
    textPair("old-version notice text on its card", find("main [role=status] p", "你正在看第 2 版"));
    return results;
  })()`);
  out.contrast.push(...measure.map((m) => ({ theme, ...m })));
  await c.context().close();
}
await browser.close();
writeFileSync("/out/flow-result.json", JSON.stringify(out, null, 1));
for (const f of out.flows) console.log(f.name, "csp:", f.csp.length, "console:", f.console.length, "pageErrors:", f.pageErrors.length, "sandbox requests:", f.sandbox.length, "|", f.steps.filter((s) => /focus|overflow|header/.test(s)).join(" ; "));
for (const m of out.contrast) console.log(m.theme, m.name, m.ratio ?? m.error, m.need ?? "", m.ratio !== undefined && m.ratio < m.need ? "FAIL" : "");
