// Where the source panel is after it opens (Laysi/trustiot-v2#1184), run in the Playwright image
// (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`). Adapted from the
// Laysi/trustiot-v2#1197 and #1182 walkthroughs' browser-check.mjs.
//
// In Traditional Chinese (zh-TW, Asia/Taipei, zoom 100 %, light theme) at CSS viewports of
// 1920×1080, 1280×720, 1280×600, 1024×576 and 853×400, and at 1280×720 with
// `prefers-reduced-motion: reduce`, it opens the source panel the ways a presenter does:
//   dashboard page, 「Line 2 power statistics」: the chart 「Daily average power」 by its
//     「資料來源」 button, by a click on a data point and by Enter on its slider;
//   dashboard page, 「Plant energy overview」: the KPI 「Energy this week」 by its number, and the
//     chart 「Power by line」 by its 「資料來源」 button;
//   conversation page, the last answer's card and the draft panel: the KPI 「Energy, week of
//     Oct 26」 by its number, the chart 「Hourly average power on Wednesday」 by its 「資料來源」
//     button, and (answer card) by a click on a data point and by Enter.
// Before each opening the component's top is scrolled just under the top bar (a presenter shows
// the component, then clicks); the click or key press is made without the browser scrolling for
// it (a data point is clicked where the chart is inside the band between the bars).
//
// Every opening prints one JSON line, measured after the page stopped scrolling (the scroll
// position unchanged for SETTLE_FRAMES animation frames in a row): the band between the sticky bars
// (the page scroller's scroll-padding), the panel's box, how many of its pixels and what share of
// its height are inside the band, whether the whole panel is, whether the panel's top is, whether
// the focused heading is, where focus is, on the dashboard page whether the panel's 「請 AI 解釋」
// button is, the scroll position right after the click against settled (a smooth scroll is still
// moving right after the click, an instant one is not) and the console errors since the last
// line. A close with the panel's close button prints where focus went. Screenshots (JPEG) are
// printed base64 after the JSON lines when SHOTS=1; they are never part of the committed record.
// The preview's own toolbar is hidden.
//
// Usage: docs/test-reports/data/2026-11-07-1184-source-panel-into-view/run.sh (from the repository
// root). VIEWPORTS=<id,id> limits the run to some viewports; SHOTS=1 takes screenshots.
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
/** Animation frames with an unchanged scroll position that count as the page having stopped. */
const SETTLE_FRAMES = 20;
const PREVIEW_CONVERSATION = "/conversations/3e7b1c94-8a52-4d60-b9f3-0c6d2a5e7f18";
/** What the preview holds (src/test/fake-conversations.ts, fake-dashboards.ts). */
const DRAFT_TITLE = "Line 2 power draft";
const V2_KPI = "Energy, week of Oct 26";
const V2_CHART = "Hourly average power on Wednesday";

const ALL = [
  { id: "1920x1080", width: 1920, height: 1080, reduced: false },
  { id: "1280x720", width: 1280, height: 720, reduced: false },
  { id: "1280x600", width: 1280, height: 600, reduced: false },
  { id: "1024x576", width: 1024, height: 576, reduced: false },
  { id: "853x400", width: 853, height: 400, reduced: false },
  { id: "1280x720-reduced", width: 1280, height: 720, reduced: true },
];
const only = (process.env.VIEWPORTS ?? "").split(",").filter(Boolean);
const VIEWPORTS = only.length > 0 ? ALL.filter((viewport) => only.includes(viewport.id)) : ALL;
const TAKE_SHOTS = process.env.SHOTS === "1";

const browser = await chromium.launch();
const failures = [];
const consoleErrors = [];
const shots = [];

function emit(viewport, step, data) {
  const where = viewport
    ? { viewport: viewport.id, css: `${viewport.width}x${viewport.height}`, reducedMotion: viewport.reduced }
    : {};
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

/** Runs in the page: the band between the sticky bars, from the page scroller's scroll-padding. */
function bandOf() {
  const root = document.documentElement;
  const style = getComputedStyle(root);
  const top = Number.parseFloat(style.scrollPaddingTop) || 0;
  const bottom = root.clientHeight - (Number.parseFloat(style.scrollPaddingBottom) || 0);
  return { top, bottom };
}

/**
 * Runs in the page. `panel` may be null (nothing open). For the panel: its box, the pixels of it
 * inside the band, the heading, focus and the page's scroll position. `explain` is the panel's
 * 「請 AI 解釋」 button, when it has one.
 */
function inPage([panel, explain]) {
  // Same as bandOf: a function passed to the page is serialized alone, so it cannot call another.
  const style = getComputedStyle(document.documentElement);
  const band = {
    top: Number.parseFloat(style.scrollPaddingTop) || 0,
    bottom: document.documentElement.clientHeight - (Number.parseFloat(style.scrollPaddingBottom) || 0),
  };
  const inside = (rect) => rect.top >= band.top - 0.5 && rect.bottom <= band.bottom + 0.5;
  const out = {
    window: { width: document.documentElement.clientWidth, height: document.documentElement.clientHeight },
    band: { top: Math.round(band.top), bottom: Math.round(band.bottom), height: Math.round(band.bottom - band.top) },
    scrollY: Math.round(window.scrollY),
  };
  const active = document.activeElement;
  const heading = panel ? panel.querySelector("h4") : null;
  out.focus =
    active && active !== document.body
      ? {
          tag: active.tagName.toLowerCase(),
          name: (active.getAttribute("aria-label") ?? active.textContent ?? "").replace(/\s+/g, " ").trim().slice(0, 50),
          isPanelHeading: heading !== null && active === heading,
          inBand: inside(active.getBoundingClientRect()),
        }
      : { tag: "body", isPanelHeading: false, inBand: false };
  if (panel) {
    const rect = panel.getBoundingClientRect();
    const visible = Math.max(0, Math.min(rect.bottom, band.bottom) - Math.max(rect.top, band.top));
    out.panel = {
      top: Math.round(rect.top),
      bottom: Math.round(rect.bottom),
      height: Math.round(rect.height),
      visiblePx: Math.round(visible),
      visibleShare: Math.round((visible / rect.height) * 100) / 100,
      fitsInBand: rect.height <= band.bottom - band.top + 0.5,
      wholeInBand: inside(rect),
      topInBand: rect.top >= band.top - 0.5 && rect.top < band.bottom,
    };
    if (heading) out.headingInBand = inside(heading.getBoundingClientRect());
  }
  if (explain) {
    const rect = explain.getBoundingClientRect();
    out.explainInBand = inside(rect);
    out.explain = { top: Math.round(rect.top), bottom: Math.round(rect.bottom) };
  }
  return out;
}

/** Runs in the page: resolves with the scroll position once it was the same for `frames` frames in a row. */
function settle(frames) {
  return new Promise((resolve) => {
    let last = window.scrollY;
    let still = 0;
    const tick = () => {
      if (window.scrollY === last) still += 1;
      else {
        last = window.scrollY;
        still = 0;
      }
      if (still >= frames) resolve(Math.round(window.scrollY));
      else requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
}

async function walk(viewport) {
  const context = await browser.newContext({
    locale: "zh-TW",
    timezoneId: "Asia/Taipei",
    viewport: { width: viewport.width, height: viewport.height },
    deviceScaleFactor: 1,
    colorScheme: "light",
    reducedMotion: viewport.reduced ? "reduce" : "no-preference",
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
   * One opening. `open` does the click or key press on `card` (called with the card's locator);
   * the panel is then measured once the page stopped scrolling, and closed with its close button.
   */
  const openSource = async (card, title, where, shotName, open, { explain = false } = {}) => {
    await waitVisible(card, `${title} in ${where}`);
    await scrollUnderHeader(card);
    await page.evaluate(settle, SETTLE_FRAMES);
    const scrollBefore = await page.evaluate(() => Math.round(window.scrollY));
    await open(card);
    const source = card.getByRole("region", { name: "資料來源" });
    await waitVisible(source, `the source panel in ${where}`);
    const rightAfter = await page.evaluate(() => Math.round(window.scrollY));
    await waitVisible(source.getByText("筆數", { exact: true }).first(), `the panel's 筆數 line in ${where}`);
    await page.evaluate(() => document.fonts.ready);
    const settledY = await page.evaluate(settle, SETTLE_FRAMES);
    const panel = await source.elementHandle();
    const explainButton = explain ? source.getByRole("button", { name: /請 AI 解釋/ }) : null;
    if (explainButton) await waitVisible(explainButton, `the panel's 請 AI 解釋 in ${where}`);
    const explainHandle = explainButton ? await explainButton.elementHandle() : null;
    const measured = await page.evaluate(inPage, [panel, explainHandle]);
    emit(viewport, `${where}: opened`, {
      scrollBefore,
      scrollRightAfterClick: rightAfter,
      scrollSettled: settledY,
      scrollStillMovingAfterClick: rightAfter !== settledY,
      ...measured,
    });
    await shoot(page, viewport, shotName);
    const close = source.getByRole("button", { name: "關閉來源" });
    await close.click();
    await source.waitFor({ state: "hidden", timeout: WAIT_MS });
    await page.evaluate(settle, SETTLE_FRAMES);
    const closed = await page.evaluate(inPage, [null, null]);
    emit(viewport, `${where}: closed`, { focusAfterClose: closed.focus, scrollY: closed.scrollY });
  };

  const byButton = (title) => (card) => card.getByRole("button", { name: `「${title}」的資料來源` }).click();
  const byNumber = (card) => card.getByRole("button", { name: /查看來源$/ }).first().click();
  /** The slider is focused without scrolling, as Tab would reach it with the chart in view. */
  const byEnter = async (card) => {
    const slider = card.getByRole("slider").first();
    await slider.evaluate((element) => element.focus({ preventScroll: true }));
    await page.keyboard.press("Enter");
  };
  /**
   * A click on the plot where the chart is inside the band between the bars: the pointer goes
   * there first, so the chart has a tooltip for the point under it, as a real click does.
   */
  const byPoint = async (card) => {
    const chart = card.locator("[data-testid=chart-root]").first();
    await waitVisible(chart.locator("canvas").first(), "the chart's canvas");
    const box = await chart.boundingBox();
    const band = await page.evaluate(bandOf);
    const top = Math.max(box.y, band.top);
    const bottom = Math.min(box.y + box.height, band.bottom);
    if (bottom - top < 20) throw new Error(`the chart has only ${Math.round(bottom - top)} px inside the band to click in`);
    const x = box.x + box.width / 2;
    const y = (top + bottom) / 2;
    await page.mouse.move(x - 40, y);
    await page.mouse.move(x, y);
    await waitVisible(page.getByTestId("chart-tooltip"), "the chart's tooltip under the pointer");
    await page.mouse.click(x, y);
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

  const conversation = [
    ["answer", card2, "(c1)"],
    ["draft panel", draftPanel, "(c2)"],
  ];
  for (const [name, root, tag] of conversation) {
    await step(`conversation ${name}: KPI number`, () =>
      openSource(component(root, V2_KPI), V2_KPI, `${tag} conversation ${name}, KPI number`, `${tag}-kpi`, byNumber),
    );
    await step(`conversation ${name}: chart button`, () =>
      openSource(component(root, V2_CHART), V2_CHART, `${tag} conversation ${name}, chart 「資料來源」 button`, `${tag}-chart`, byButton(V2_CHART)),
    );
  }
  await step("conversation answer: chart data point click", () =>
    openSource(component(card2, V2_CHART), V2_CHART, "(c1) conversation answer, chart data point click", "c1-point", byPoint),
  );
  await step("conversation answer: chart Enter", () =>
    openSource(component(card2, V2_CHART), V2_CHART, "(c1) conversation answer, chart Enter on the slider", "c1-enter", byEnter),
  );

  const dashboard = async (name) => {
    await page.goto(`${BASE}/dashboards`);
    const link = main.getByRole("link", { name, exact: true });
    await waitVisible(link, `the list's link to ${name}`);
    await link.click();
    await page.waitForURL(/\/dashboards\/[0-9a-f-]+/, { timeout: WAIT_MS });
  };
  await step("dashboard page: Line 2 power statistics", async () => {
    await dashboard("Line 2 power statistics");
    const title = "Daily average power";
    const chart = () => component(main, title);
    await waitVisible(chart().locator("canvas").first(), `the chart ${title}`);
    await openSource(chart(), title, "(d1) dashboard page, Line 2 power statistics, chart 「資料來源」 button", "d1-chart", byButton(title), { explain: true });
    await openSource(chart(), title, "(d2) dashboard page, Line 2 power statistics, chart data point click", "d2-point", byPoint, { explain: true });
    await openSource(chart(), title, "(d3) dashboard page, Line 2 power statistics, chart Enter on the slider", "d3-enter", byEnter, { explain: true });
  });
  await step("dashboard page: Plant energy overview", async () => {
    await dashboard("Plant energy overview");
    await openSource(component(main, "Energy this week"), "Energy this week", "(d4) dashboard page, Plant energy overview, KPI number", "d4-kpi", byNumber, { explain: true });
    await openSource(component(main, "Power by line"), "Power by line", "(d5) dashboard page, Plant energy overview, chart 「資料來源」 button", "d5-chart", byButton("Power by line"), { explain: true });
  });

  await context.close();
}

for (const viewport of VIEWPORTS) {
  await walk(viewport);
}
await browser.close();
emit(null, "summary", { viewports: VIEWPORTS.map((viewport) => `${viewport.id} ${viewport.width}x${viewport.height}`), failures });

if (shots.length > 0) {
  console.log("=== SCREENSHOTS BEGIN ===");
  for (const shot of shots) console.log(`SHOT ${shot.name} ${shot.base64}`);
  console.log("=== SCREENSHOTS END ===");
}
process.exitCode = failures.length > 0 ? 1 : 0;
