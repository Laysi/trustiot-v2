// Why does going back to a conversation not restore the scroll position (Laysi/trustiot-v2#1185)?
// Records, from just before `goBack()` until the page has been still, every scroll event (scrollY,
// scrollHeight, time, active element), every window.scrollTo / scrollBy / scrollIntoView / focus
// call with a short stack, and the page height at first render and at each step, for the three
// conversation cases of browser-check.mjs. Prints one JSON line per case.
//
// Usage: TRACE=1 docs/test-reports/data/2026-11-07-1185-scroll-reset/run.sh (from the repository
// root; run.sh runs this file instead of browser-check.mjs when TRACE is set).
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
const WAIT_MS = 60_000;
const PREVIEW_CONVERSATION = "/conversations/3e7b1c94-8a52-4d60-b9f3-0c6d2a5e7f18";
const browser = await chromium.launch();

function instrument() {
  const log = [];
  const t0 = performance.now();
  const now = () => Math.round(performance.now() - t0);
  const stack = () =>
    (new Error().stack ?? "")
      .split("\n")
      .slice(3, 6)
      .map((line) => line.trim().replace(/https?:\/\/[^/]+/, ""))
      .join(" < ");
  const element = (node) => (node ? `${node.tagName?.toLowerCase()}${node.id ? `#${node.id}` : ""}` : null);
  const height = () => document.documentElement.scrollHeight;
  window.__trace = { log, on: false, now, height };
  const record = (entry) => {
    if (window.__trace.on) log.push({ t: now(), y: Math.round(window.scrollY), h: height(), ...entry });
  };
  for (const name of ["scrollTo", "scrollBy"]) {
    const original = window[name].bind(window);
    window[name] = (...args) => {
      record({ call: name, args: JSON.stringify(args), stack: stack() });
      return original(...args);
    };
  }
  const intoView = Element.prototype.scrollIntoView;
  Element.prototype.scrollIntoView = function (...args) {
    record({ call: "scrollIntoView", on: element(this), args: JSON.stringify(args), stack: stack() });
    return intoView.apply(this, args);
  };
  const focus = HTMLElement.prototype.focus;
  HTMLElement.prototype.focus = function (...args) {
    record({ call: "focus", on: element(this), args: JSON.stringify(args), stack: stack() });
    return focus.apply(this, args);
  };
  window.addEventListener("scroll", () => record({ event: "scroll", active: element(document.activeElement) }), { passive: true });
  // Heights of the cards' parts as they change.
  new ResizeObserver(() => record({ event: "resize" })).observe(document.documentElement);
}

async function trace(label, width, height, target) {
  const context = await browser.newContext({ locale: "zh-TW", timezoneId: "Asia/Taipei", viewport: { width, height } });
  await context.addInitScript(instrument);
  const page = await context.newPage();
  const main = page.locator("#root main");
  await page.goto(`${BASE}${PREVIEW_CONVERSATION}`);
  const card = main.locator("section[data-answer-dashboard]").first();
  await card.waitFor({ state: "visible", timeout: WAIT_MS });
  const firstRender = await page.evaluate(() => ({ height: document.documentElement.scrollHeight, canvases: document.querySelectorAll("canvas").length }));
  await card.locator("canvas").first().waitFor({ state: "visible", timeout: WAIT_MS });
  const save = card.getByRole("button", { name: "儲存到儀表板列表" });
  const unsavedHeight = await page.evaluate(() => document.documentElement.scrollHeight);
  await save.click();
  await card.getByText(/已儲存為儀表板列表裡的第 \d+ 版/).waitFor({ state: "visible", timeout: WAIT_MS });
  const open = card.getByRole("link", { name: "開啟儀表板" });
  await open.waitFor({ state: "visible", timeout: WAIT_MS });
  const savedHeight = await page.evaluate(() => document.documentElement.scrollHeight);
  const max = await page.evaluate(() => document.documentElement.scrollHeight - window.innerHeight);
  let y = target === "max" ? max : Math.min(target, max);
  const covered = async () => {
    const box = await open.boundingBox();
    if (!box) return true;
    return page.evaluate(([x, py]) => !document.elementFromPoint(x, py)?.closest("a[href]"), [box.x + box.width / 2, box.y + box.height / 2]);
  };
  await page.evaluate((to) => window.scrollTo(0, to), y);
  // The sticky composer can cover the link at the very bottom: go up until it can be clicked.
  while ((await covered()) && y > 0) {
    y = Math.max(0, y - 20);
    await page.evaluate((to) => window.scrollTo(0, to), y);
  }
  await page.waitForTimeout(500);
  const before = await page.evaluate(() => ({ y: Math.round(window.scrollY), h: document.documentElement.scrollHeight }));
  await page.evaluate(() => {
    window.__trace.log.length = 0;
    window.__trace.on = true;
  });
  const box = await open.boundingBox();
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  await main.getByRole("heading", { level: 1 }).waitFor({ state: "visible", timeout: WAIT_MS });
  await page.getByText(/^資料時間/).first().waitFor({ state: "visible", timeout: WAIT_MS });
  await page.waitForTimeout(500);
  const leaving = await page.evaluate(() => ({
    log: window.__trace.log.splice(0),
    saved: sessionStorage.getItem("react-router-scroll-positions"),
  }));
  await page.goBack();
  await card.waitFor({ state: "visible", timeout: WAIT_MS });
  // Until the page has been still: no log entry and no height change for 1.5 s, within a bound.
  const deadline = Date.now() + WAIT_MS;
  let lastCount = -1;
  let lastChange = Date.now();
  while (Date.now() < deadline && Date.now() - lastChange < 1500) {
    const count = await page.evaluate(() => window.__trace.log.length);
    if (count !== lastCount) {
      lastCount = count;
      lastChange = Date.now();
    }
    await page.waitForTimeout(100);
  }
  const after = await page.evaluate(() => ({
    y: Math.round(window.scrollY),
    h: document.documentElement.scrollHeight,
    saveOutcomeVisible: !!document.querySelector("section[data-answer-dashboard] a[href^='/dashboards/']"),
    loadingCards: document.querySelectorAll("[aria-busy='true']").length,
    canvases: document.querySelectorAll("canvas").length,
  }));
  const log = await page.evaluate(() => window.__trace.log);
  console.log(JSON.stringify({ case: label, firstRender, unsavedHeight, savedHeight, max, before, after, leaving, entries: log.length, log: log.slice(0, 60) }));
  await context.close();
}

await trace("1280x600 at 225", 1280, 600, 225);
await trace("1920x1080 at max", 1920, 1080, "max");
await trace("1920x1080 at 400", 1920, 1080, 400);
await browser.close();
