// Closing the source panel (Laysi/trustiot-v2#1201), run in the Playwright image
// (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`). Adapted from the
// Laysi/trustiot-v2#1184 walkthrough's browser-check.mjs.
//
// In Traditional Chinese (zh-TW, Asia/Taipei, zoom 100 %, light theme), at CSS viewports of
// 1280×720 and 1920×1080 (VIEWPORTS=<id,id> limits the run), it opens and closes the source panel
// the ways a presenter and a keyboard user do, on:
//   dashboard page, 「Plant energy overview」: the KPI 「Energy this week」 and the chart 「Power
//     by line」;
//   conversation page, the last answer's card: the KPI 「Energy, week of Oct 26」 and the chart
//     「Hourly average power on Wednesday」.
// The scenarios (each one prints one JSON line per observation; nothing is asserted here, the
// report reads the lines, so the same script runs on the tree before and after the change):
//   kpi-escape        click the number, Escape inside the panel
//   kpi-press-again   click the number, click the number again
//   kpi-source-button click the number, click the card's 「資料來源」 button (the same source)
//   kpi-keyboard      focus the number, Enter; focus it again, Enter
//   chart-mouse-close hover and click a data point, close with the mouse (button), then move the
//                     pointer away
//   chart-escape      hover and click a data point, Escape inside the panel
//   chart-point-again hover and click a data point, then click the same point again
//   chart-slider-esc  open with the card's 「資料來源」 button, focus the chart (after a key press,
//                     as Shift+Tab does), Escape
// An observation is the card after the step: whether its panel is open, where focus is (and
// whether that is inside the panel), the chart tooltip's text (null when there is none), and how
// many triggers say `aria-expanded` true and false. Screenshots (JPEG) are printed base64 after
// the JSON lines when SHOTS=1; they are never part of the committed record. The preview's own
// toolbar is hidden.
//
// Usage: docs/test-reports/data/2026-11-07-1201-source-panel-close/run.sh (from the repository
// root).
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5201";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
/** Animation frames with an unchanged scroll position that count as the page having stopped. */
const SETTLE_FRAMES = 20;
const PREVIEW_CONVERSATION = "/conversations/3e7b1c94-8a52-4d60-b9f3-0c6d2a5e7f18";
const V2_KPI = "Energy, week of Oct 26";
const V2_CHART = "Hourly average power on Wednesday";

const ALL = [
  { id: "1280x720", width: 1280, height: 720 },
  { id: "1920x1080", width: 1920, height: 1080 },
];
const only = (process.env.VIEWPORTS ?? "").split(",").filter(Boolean);
const VIEWPORTS = only.length > 0 ? ALL.filter((viewport) => only.includes(viewport.id)) : ALL;
const TAKE_SHOTS = process.env.SHOTS === "1";

const browser = await chromium.launch();
const failures = [];
const consoleErrors = [];
const shots = [];

function emit(viewport, step, data) {
  const where = viewport ? { viewport: viewport.id } : {};
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

/** Runs in the page with the card: its panel, focus, tooltip and the triggers' aria-expanded. */
function observe(card) {
  const panel = card.querySelector("[data-source-panel]");
  const active = document.activeElement;
  const tip = card.querySelector("[data-testid=chart-tooltip]");
  const triggers = [...card.querySelectorAll("[aria-expanded]")];
  const focus =
    active && active !== document.body
      ? {
          tag: active.tagName.toLowerCase(),
          role: active.getAttribute("role"),
          name: (active.getAttribute("aria-label") ?? active.textContent ?? "").replace(/\s+/g, " ").trim().slice(0, 50),
          inPanel: panel !== null && panel.contains(active),
          inCard: card.contains(active),
        }
      : { tag: "body", role: null, name: "", inPanel: false, inCard: false };
  return {
    panelOpen: panel !== null,
    focus,
    tooltip: tip ? tip.textContent.replace(/\s+/g, " ").trim().slice(0, 70) : null,
    expandedTrue: triggers.filter((element) => element.getAttribute("aria-expanded") === "true").length,
    expandedFalse: triggers.filter((element) => element.getAttribute("aria-expanded") === "false").length,
  };
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
  });
  await context.addInitScript(hideToolbar);
  const page = await context.newPage();
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text().slice(0, 300));
  });
  page.on("pageerror", (error) => consoleErrors.push(`pageerror: ${error.message.slice(0, 300)}`));
  const main = page.locator("#root main");
  const component = (root, title) =>
    root.getByRole("heading", { level: 3, name: title }).locator("xpath=ancestor::section[1]");
  const scrollUnderHeader = (locator) =>
    locator.evaluate((element) => {
      const header = document.querySelector("#root header.sticky");
      window.scrollBy(0, element.getBoundingClientRect().top - (header?.getBoundingClientRect().height ?? 0) - 8);
    });
  const pause = async () => {
    await page.waitForTimeout(250);
    await page.evaluate(settle, SETTLE_FRAMES);
  };
  const observeCard = (card) => card.evaluate(observe);

  /** Runs one scenario; a failure (a timeout) is recorded and the card is closed for the next. */
  const scenario = async (where, name, card, body) => {
    try {
      await waitVisible(card, `${where}`);
      await scrollUnderHeader(card);
      await page.evaluate(settle, SETTLE_FRAMES);
      const seen = async (what, shot) => {
        await pause();
        emit(viewport, `${where} / ${name} / ${what}`, await observeCard(card));
        if (shot) await shoot(page, viewport, `${where.replace(/\W+/g, "-")}--${name}--${shot}`);
      };
      await body(seen);
    } catch (error) {
      failures.push(`${viewport.id} ${where} ${name}: ${error.message.split("\n")[0]}`);
      emit(viewport, `${where} / ${name} FAILED`, { error: error.message.split("\n")[0] });
    }
    // Back to a closed panel for the next scenario, whatever happened.
    const close = card.getByRole("button", { name: "關閉來源" });
    if ((await close.count()) > 0) await close.click().catch(() => {});
    await page.mouse.move(5, 5);
    await pause();
  };

  const panelOf = (card) => card.getByRole("region", { name: "資料來源" });
  const number = (card) => card.getByRole("button", { name: /查看來源$/ }).first();
  const sourceButton = (card, title) => card.getByRole("button", { name: `「${title}」的資料來源` });

  /** A click on the plot, after hovering it so the chart has a tooltip for the point under the pointer. */
  const clickPoint = async (card) => {
    const chart = card.locator("[data-testid=chart-root]").first();
    await waitVisible(chart.locator("canvas").first(), "the chart's canvas");
    // The card's top just under the top bar, as a presenter shows it, so the plot is in the band.
    await scrollUnderHeader(card);
    await page.evaluate(settle, SETTLE_FRAMES);
    const box = await chart.boundingBox();
    const band = await page.evaluate(bandOf);
    const top = Math.max(box.y, band.top);
    const bottom = Math.min(box.y + box.height, band.bottom);
    if (bottom - top < 20) throw new Error(`the chart has only ${Math.round(bottom - top)} px inside the band to click in`);
    const x = box.x + box.width / 2;
    const y = (top + bottom) / 2;
    await page.mouse.move(x - 40, y);
    await page.mouse.move(x, y);
    await waitVisible(card.getByTestId("chart-tooltip"), "the chart's tooltip under the pointer");
    await page.mouse.click(x, y);
  };

  const kpi = async (where, card) => {
    await scenario(where, "kpi-escape", card, async (seen) => {
      await number(card).click();
      await waitVisible(panelOf(card), "the source panel");
      await seen("opened by the number", "opened");
      await page.keyboard.press("Escape");
      await seen("after Escape in the panel", "escape");
    });
    await scenario(where, "kpi-press-again", card, async (seen) => {
      await number(card).click();
      await waitVisible(panelOf(card), "the source panel");
      await seen("opened by the number");
      await pause();
      await number(card).click();
      await seen("after the number again");
    });
    await scenario(where, "kpi-source-button", card, async (seen) => {
      await number(card).click();
      await waitVisible(panelOf(card), "the source panel");
      await seen("opened by the number");
      await pause();
      await card.getByRole("button", { name: /的資料來源$/ }).first().click();
      await seen("after the card's 資料來源 button (the same source)");
    });
    await scenario(where, "kpi-keyboard", card, async (seen) => {
      await number(card).evaluate((element) => element.focus({ preventScroll: true }));
      await page.keyboard.press("Enter");
      await waitVisible(panelOf(card), "the source panel");
      await seen("opened by Enter on the number");
      await number(card).evaluate((element) => element.focus({ preventScroll: true }));
      await page.keyboard.press("Enter");
      await seen("after Enter on the number again");
    });
  };

  const chart = async (where, card, title) => {
    await scenario(where, "chart-mouse-close", card, async (seen) => {
      await clickPoint(card);
      await waitVisible(panelOf(card), "the source panel");
      await seen("opened by a click on a point", "opened");
      await panelOf(card).getByRole("button", { name: "關閉來源" }).click();
      await seen("closed with the mouse (pointer still where the button was)", "closed-pointer-there");
      await page.mouse.move(5, 5);
      await seen("pointer moved away", "closed-pointer-away");
    });
    await scenario(where, "chart-escape", card, async (seen) => {
      await clickPoint(card);
      await waitVisible(panelOf(card), "the source panel");
      await seen("opened by a click on a point");
      await page.keyboard.press("Escape");
      await page.mouse.move(5, 5);
      await seen("after Escape in the panel", "escape");
    });
    await scenario(where, "chart-point-again", card, async (seen) => {
      await clickPoint(card);
      await waitVisible(panelOf(card), "the source panel");
      await seen("opened by a click on a point");
      await pause();
      await clickPoint(card);
      await seen("after a click on the same point again");
    });
    await scenario(where, "chart-slider-esc", card, async (seen) => {
      await sourceButton(card, title).click();
      await waitVisible(panelOf(card), "the source panel");
      await seen("opened by the card's 資料來源 button");
      // A key press, then the focus: what Shift+Tab onto the chart does, without depending on the
      // tab order (Tab is the key that reaches the chart in the browser).
      await page.keyboard.press("Shift");
      await card.getByRole("slider").first().evaluate((element) => element.focus({ preventScroll: true }));
      await seen("chart focused with the keyboard");
      await page.keyboard.press("Escape");
      await seen("after Escape on the chart", "chart-escape");
    });
  };

  // Dashboard page.
  try {
    await page.goto(`${BASE}/dashboards`);
    const link = main.getByRole("link", { name: "Plant energy overview", exact: true });
    await waitVisible(link, "the list's link to Plant energy overview");
    await link.click();
    await page.waitForURL(/\/dashboards\/[0-9a-f-]+/, { timeout: WAIT_MS });
    await waitVisible(component(main, "Power by line").locator("canvas").first(), "the chart Power by line");
    await kpi("dashboard page, KPI", component(main, "Energy this week"));
    await chart("dashboard page, chart", component(main, "Power by line"), "Power by line");
  } catch (error) {
    failures.push(`${viewport.id} dashboard page: ${error.message.split("\n")[0]}`);
    emit(viewport, "dashboard page FAILED", { error: error.message.split("\n")[0] });
  }

  // The last answer of the preview conversation.
  try {
    await page.goto(`${BASE}${PREVIEW_CONVERSATION}`);
    const cards = main.locator("section[data-answer-dashboard]");
    await page.waitForFunction(
      () => document.querySelectorAll("#root main section[data-answer-dashboard]").length === 2,
      null,
      { timeout: WAIT_MS },
    );
    const card2 = cards.nth(1);
    await component(card2, V2_CHART).scrollIntoViewIfNeeded();
    await waitVisible(component(card2, V2_CHART).locator("canvas").first(), "the last answer's chart");
    await kpi("conversation answer, KPI", component(card2, V2_KPI));
    await chart("conversation answer, chart", component(card2, V2_CHART), V2_CHART);
  } catch (error) {
    failures.push(`${viewport.id} conversation answer: ${error.message.split("\n")[0]}`);
    emit(viewport, "conversation answer FAILED", { error: error.message.split("\n")[0] });
  }

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
