// The width of the source panel opened from a one-cell component (Laysi/trustiot-v2#1197), run in
// the Playwright image (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`) of
// a tree that has the preview's draft panel (Laysi/trustiot-v2#1195's preview changes). Adapted
// from the Laysi/trustiot-v2#1195 walkthrough's browser-check.mjs.
//
// In Traditional Chinese (zh-TW, Asia/Taipei, zoom 100 %), at CSS viewports of 1280×720, 1536×768
// and 1920×1080 (the conversation page is two columns from `xl`, 1280 px), at 1279×720 (one
// column) and at 1280×720 in the dark theme, it opens the source panel from the number of:
//   (a) the KPI 「Energy, week of Oct 26」 in the draft panel of the preview conversation;
//   (b) the same KPI in the last answer's card;
//   (c) on the dashboard page: the same KPI (a usage query) after saving the draft from the last
//       answer and following 「開啟儀表板」; the computed KPI 「Energy this week」 of 「Plant energy
//       overview」; the status 「Meter 1」 (an SQL dataset) of 「Daily meter readings」.
// Each opening prints one JSON line: the panel's box, the grid item's and the grid's widths before
// and after, where the number that opened it moved, text that runs at three characters or fewer a
// line, truncated text in the panel, the page's horizontal overflow and where focus is. Closing
// prints one more: where focus went and whether the item is back in its cell. Screenshots (JPEG)
// are printed base64 after the JSON lines, between markers, for the reviewer's eyes; they are
// never part of the committed record. The preview's own toolbar is hidden.
//
// Usage: docs/test-reports/data/2026-11-07-1197-source-panel-width/run.sh (from the repository
// root). VIEWPORTS=<id,id> limits the run to some viewports; SHOTS=0 takes no screenshots.
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
const PREVIEW_CONVERSATION = "/conversations/3e7b1c94-8a52-4d60-b9f3-0c6d2a5e7f18";
/** What the preview holds (src/test/fake-conversations.ts, fake-dashboards.ts). */
const DRAFT_TITLE = "Line 2 power draft";
const V2_KPI = "Energy, week of Oct 26";
const V2_CHART = "Hourly average power on Wednesday";

const ALL = [
  { id: "1280x720", width: 1280, height: 720, scheme: "light" },
  { id: "1536x768", width: 1536, height: 768, scheme: "light" },
  { id: "1920x1080", width: 1920, height: 1080, scheme: "light" },
  { id: "1279x720", width: 1279, height: 720, scheme: "light" },
  { id: "1280x720-dark", width: 1280, height: 720, scheme: "dark" },
];
const only = (process.env.VIEWPORTS ?? "").split(",").filter(Boolean);
const VIEWPORTS = only.length > 0 ? ALL.filter((viewport) => only.includes(viewport.id)) : ALL;
const TAKE_SHOTS = process.env.SHOTS !== "0";

const browser = await chromium.launch();
const failures = [];
const consoleErrors = [];
const shots = [];

function emit(viewport, step, data) {
  const where = viewport ? { viewport: viewport.id, css: `${viewport.width}x${viewport.height}`, scheme: viewport.scheme } : {};
  console.log(JSON.stringify({ step, ...where, ...data, consoleErrors: [...consoleErrors] }));
  consoleErrors.length = 0;
}

async function waitVisible(locator, what, timeout = WAIT_MS) {
  try {
    await locator.waitFor({ state: "visible", timeout });
  } catch {
    throw new Error(`timed out after ${timeout / 1000} s waiting for ${what}`);
  }
}

async function shoot(page, viewport, name) {
  if (!TAKE_SHOTS) return;
  const image = await page.screenshot({ type: "jpeg", quality: 50, scale: "css" });
  shots.push({ name: `${viewport.id}--${name}`, base64: image.toString("base64") });
}

/** Hides the preview's toolbar above #root, which the product does not draw. */
function hideToolbar() {
  document.addEventListener("DOMContentLoaded", () => {
    const style = document.createElement("style");
    style.textContent = "#ui-preview-toolbar{display:none!important}";
    document.head.append(style);
  });
}

/**
 * Runs in the page. The band between the sticky header and the conversation's sticky composer,
 * the page's horizontal overflow, and for each handle its box. For `panel`: text that runs at
 * three characters or fewer a line (three lines or more), text cut by its own box, the column
 * names of the clicked rows, and the panel's height against the band.
 */
function inPage([handles, panel]) {
  const vw = document.documentElement.clientWidth;
  const vh = window.innerHeight;
  const header = document.querySelector("#root header.sticky");
  const headerBottom = header ? header.getBoundingClientRect().bottom : 0;
  const composer = [...document.querySelectorAll("#root .sticky")].find((candidate) => {
    const rect = candidate.getBoundingClientRect();
    return candidate.tagName !== "HEADER" && !candidate.closest("table") && rect.bottom >= vh - 1 && rect.top > vh / 3;
  });
  const bandBottom = composer ? composer.getBoundingClientRect().top : vh;
  const box = (element) => {
    if (!element) return null;
    const rect = element.getBoundingClientRect();
    return {
      left: Math.round(rect.left),
      width: Math.round(rect.width),
      top: Math.round(rect.top),
      height: Math.round(rect.height),
      inBand: rect.top >= headerBottom - 1 && rect.bottom <= bandBottom + 1,
    };
  };
  const out = {
    band: { top: Math.round(headerBottom), bottom: Math.round(bandBottom) },
    horizontalScroll: document.scrollingElement.scrollWidth > vw,
    scrollY: Math.round(window.scrollY),
    boxes: Object.fromEntries(Object.entries(handles).map(([name, element]) => [name, box(element)])),
  };
  const active = document.activeElement;
  out.focus =
    active && active !== document.body
      ? {
          tag: active.tagName.toLowerCase(),
          name: (active.getAttribute("aria-label") ?? active.textContent ?? "").replace(/\s+/g, " ").trim().slice(0, 60),
          ...box(active),
          isTrigger: active === handles.trigger,
          inPanel: panel ? panel.contains(active) : false,
        }
      : { tag: "body" };
  if (panel) {
    const narrow = [];
    let maxLines = 0;
    const walker = document.createTreeWalker(panel, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const text = node.textContent.replace(/\s+/g, " ").trim();
      if (text.length < 4 || node.parentElement.closest(".sr-only")) continue;
      const range = document.createRange();
      range.selectNodeContents(node);
      const lines = new Set([...range.getClientRects()].filter((rect) => rect.width > 0).map((rect) => Math.round(rect.top))).size;
      maxLines = Math.max(maxLines, lines);
      if (lines >= 3 && text.length / lines <= 3) {
        narrow.push({ text: text.slice(0, 40), chars: text.length, lines, width: Math.round(node.parentElement.getBoundingClientRect().width) });
      }
    }
    const truncated = [];
    for (const element of panel.querySelectorAll("*")) {
      const style = getComputedStyle(element);
      const clips = style.textOverflow === "ellipsis" || ["hidden", "clip"].includes(style.overflowX);
      if (clips && element.scrollWidth > element.clientWidth + 1) {
        truncated.push({ tag: element.tagName.toLowerCase(), text: element.textContent.trim().slice(0, 40), clientWidth: element.clientWidth, scrollWidth: element.scrollWidth });
      }
    }
    const columns = [...panel.querySelectorAll("section[aria-label] dl dt")].map((dt) => ({
      name: dt.textContent,
      clientWidth: dt.clientWidth,
      scrollWidth: dt.scrollWidth,
      cut: dt.scrollWidth > dt.clientWidth + 1,
    }));
    out.panel = {
      narrowText: narrow.slice(0, 10),
      narrowCount: narrow.length,
      maxLinesOfOneText: maxLines,
      truncated,
      columns,
      fitsInBand: panel.getBoundingClientRect().height <= bandBottom - headerBottom,
    };
  }
  return out;
}

async function walk(viewport) {
  const context = await browser.newContext({
    locale: "zh-TW",
    timezoneId: "Asia/Taipei",
    viewport: { width: viewport.width, height: viewport.height },
    deviceScaleFactor: 1,
    colorScheme: viewport.scheme,
  });
  await context.addInitScript(hideToolbar);
  const page = await context.newPage();
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text().slice(0, 300));
  });
  page.on("pageerror", (error) => consoleErrors.push(`pageerror: ${error.message.slice(0, 300)}`));
  const main = page.locator("#root main");
  const step = async (name, body) => {
    try {
      await body();
    } catch (error) {
      failures.push(`${viewport.id} ${name}: ${error.message.split("\n")[0]}`);
      // What the page shows instead: its headings and alerts.
      const shown = await page
        .locator("#root h1, #root h2, #root h3, #root [role=alert]")
        .allTextContents()
        .catch(() => []);
      emit(viewport, `${name} FAILED`, {
        error: error.message.split("\n")[0],
        url: new URL(page.url()).pathname + new URL(page.url()).search,
        shown: shown.map((text) => text.replace(/\s+/g, " ").trim().slice(0, 80)).slice(0, 20),
      });
      await shoot(page, viewport, `${name.replace(/\W+/g, "-")}-failed`);
    }
  };
  const component = (root, title) => root.getByRole("heading", { level: 3, name: title }).locator("xpath=ancestor::section[1]");
  const scrollUnderHeader = (locator) =>
    locator.evaluate((element) => {
      const header = document.querySelector("#root header.sticky");
      window.scrollBy(0, element.getBoundingClientRect().top - (header?.getBoundingClientRect().height ?? 0) - 8);
    });
  const answers = main.getByRole("article", { name: "TrustIoT 的回答" });
  const cards = main.locator("section[data-answer-dashboard]");
  const card2 = cards.nth(1);
  const panelTitle = main.getByRole("heading", { level: 2, name: DRAFT_TITLE });
  const draftPanel = panelTitle.locator("xpath=ancestor::div[contains(@class,'bg-muted')][1]");

  /**
   * Opens the source panel from the number of the component titled `title` in `root`, its top
   * scrolled just under the header first (a presenter shows the number, then clicks it), measures,
   * then closes it with its close button and measures again.
   */
  const openSource = async (root, title, where, shotName) => {
    const card = component(root, title);
    await waitVisible(card, `${title} in ${where}`);
    const button = card.getByRole("button", { name: /查看來源$/ }).first();
    await waitVisible(button, `the 查看來源 number of ${title} in ${where}`);
    await scrollUnderHeader(card);
    const item = await card.evaluateHandle((element) => element.parentElement);
    const grid = await item.evaluateHandle((element) => element.parentElement);
    const trigger = await button.elementHandle();
    const before = await page.evaluate(inPage, [{ item, grid, trigger }, null]);
    await button.click();
    const source = card.getByRole("region", { name: "資料來源" });
    await waitVisible(source, `the source panel in ${where}`);
    await waitVisible(source.getByText("筆數", { exact: true }).first(), `the panel's 筆數 line in ${where}`);
    await page.evaluate(() => document.fonts.ready);
    const panel = await source.elementHandle();
    const opened = await page.evaluate(inPage, [{ item, grid, trigger, panel }, panel]);
    emit(viewport, `${where}: opened`, {
      gridColumns: await grid.evaluate((element) => getComputedStyle(element).gridTemplateColumns.split(" ").length),
      itemColumn: await item.evaluate((element) => getComputedStyle(element).gridColumn),
      before: before.boxes,
      ...opened,
      spansGrid: opened.boxes.item.width >= opened.boxes.grid.width - 1,
      triggerMoved: { dx: opened.boxes.trigger.left - before.boxes.trigger.left, dy: opened.boxes.trigger.top - before.boxes.trigger.top },
    });
    await shoot(page, viewport, `${shotName}`);
    // A presenter who read the panel presses its close button, wherever it is.
    const close = source.getByRole("button", { name: "關閉來源" });
    await close.scrollIntoViewIfNeeded();
    await close.click();
    await source.waitFor({ state: "hidden", timeout: WAIT_MS });
    const closed = await page.evaluate(inPage, [{ item, grid, trigger }, null]);
    emit(viewport, `${where}: closed`, {
      ...closed,
      itemBackInItsCell: closed.boxes.item.width === before.boxes.item.width,
    });
    await shoot(page, viewport, `${shotName}-closed`);
  };

  await step("open the conversation", async () => {
    await page.goto(`${BASE}${PREVIEW_CONVERSATION}`);
    await waitVisible(answers.first(), "the preview conversation's first answer");
    await page
      .waitForFunction(() => document.querySelectorAll("#root main section[data-answer-dashboard]").length === 2, null, { timeout: WAIT_MS })
      .catch(() => {
        throw new Error("timed out after 60 s waiting for the two answers' dashboard cards");
      });
    await waitVisible(panelTitle, `the draft panel's title ${DRAFT_TITLE}`);
    for (const [root, what] of [
      [card2, "the last answer's chart"],
      [draftPanel, "the draft panel's chart"],
    ]) {
      await component(root, V2_CHART).scrollIntoViewIfNeeded();
      await waitVisible(component(root, V2_CHART).locator("canvas").first(), what);
    }
    emit(viewport, "conversation open", { draftsAlert: await main.getByText(/^讀不到這段對話的草稿/).count() });
  });

  await step("source from the draft panel", () => openSource(draftPanel, V2_KPI, "(a) draft panel", "a-panel"));
  await step("source from the answer", () => openSource(card2, V2_KPI, "(b) answer", "b-answer"));

  await step("source on the dashboard page: the saved draft", async () => {
    await scrollUnderHeader(card2);
    const save = card2.getByRole("button", { name: "儲存到儀表板列表" });
    await waitVisible(save, "the last answer's 儲存到儀表板列表 button");
    await save.click();
    const link = card2.getByRole("link", { name: "開啟儀表板" });
    await waitVisible(link, "the last answer's 開啟儀表板 link");
    await link.click();
    await page.waitForURL(/\/dashboards\/[0-9a-f-]+/, { timeout: WAIT_MS });
    await openSource(main, V2_KPI, "(c1) dashboard page, saved draft's KPI (usage query)", "c1-saved");
  });

  for (const [dashboard, title, label, shot] of [
    ["Plant energy overview", "Energy this week", "(c2) dashboard page, Plant energy overview's computed KPI", "c2-plant"],
    ["Daily meter readings", "Meter 1", "(c3) dashboard page, Daily meter readings' status (SQL)", "c3-status"],
  ]) {
    await step(`source on the dashboard page: ${dashboard}`, async () => {
      await page.goto(`${BASE}/dashboards`);
      const link = main.getByRole("link", { name: dashboard, exact: true });
      await waitVisible(link, `the list's link to ${dashboard}`);
      await link.click();
      await page.waitForURL(/\/dashboards\/[0-9a-f-]+/, { timeout: WAIT_MS });
      await openSource(main, title, label, shot);
    });
  }

  await context.close();
}

for (const viewport of VIEWPORTS) {
  await walk(viewport);
}
await browser.close();
emit(null, "summary", { viewports: VIEWPORTS.map((viewport) => `${viewport.id} ${viewport.width}x${viewport.height} ${viewport.scheme}`), failures });

if (shots.length > 0) {
  console.log("=== SCREENSHOTS BEGIN ===");
  for (const shot of shots) console.log(`SHOT ${shot.name} ${shot.base64}`);
  console.log("=== SCREENSHOTS END ===");
}
process.exitCode = failures.length > 0 ? 1 : 0;
