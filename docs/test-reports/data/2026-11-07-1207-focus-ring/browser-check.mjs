// Measures the keyboard focus outline of Laysi/trustiot-v2#1207 in the UI preview: Tab onto a
// button, a text input, a chart (role="slider"), a link and the device page's range toggle, and
// record the computed outline; then click a button with the mouse and record it again. Runs in the
// pinned Playwright image against the preview (run.sh), zh-TW, Asia/Taipei, 1280x720, once per
// colour scheme (light and dark, through the browser's prefers-color-scheme).
//
// Every step prints one JSON line carrying `mode` (env MODE: `before` or `after`). Screenshots
// (JPEG) are printed base64 on lines `SHOT <name> <data>`; run.sh writes them outside the
// repository.
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5207";
const MODE = process.env.MODE ?? "after";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
const SCHEMES = ["light", "dark"];
const TAB_LIMIT = 150;

const browser = await chromium.launch();
let failed = false;

function emit(scheme, step, data) {
  console.log(JSON.stringify({ mode: MODE, step, scheme, viewport: "1280x720", ...data }));
}

/** The computed outline of the focused element and whether an ancestor clips the ring. Runs in the page. */
function describeFocused() {
  const element = document.activeElement;
  if (!element || element === document.body) return null;
  const style = getComputedStyle(element);
  const rect = element.getBoundingClientRect();
  const width = Number.parseFloat(style.outlineWidth) || 0;
  const offset = Number.parseFloat(style.outlineOffset) || 0;
  const reach = style.outlineStyle === "none" ? 0 : width + offset;
  // An ancestor that clips (overflow other than visible) hides the part of the ring outside its box.
  const clippedBy = [];
  for (let parent = element.parentElement; parent && parent !== document.documentElement; parent = parent.parentElement) {
    const parentStyle = getComputedStyle(parent);
    if (parentStyle.overflowX === "visible" && parentStyle.overflowY === "visible") continue;
    const box = parent.getBoundingClientRect();
    const out = {
      left: Math.max(0, box.left - (rect.left - reach)),
      top: Math.max(0, box.top - (rect.top - reach)),
      right: Math.max(0, rect.right + reach - box.right),
      bottom: Math.max(0, rect.bottom + reach - box.bottom),
    };
    if (reach > 0 && (out.left > 0 || out.top > 0 || out.right > 0 || out.bottom > 0)) {
      clippedBy.push({
        tag: parent.tagName.toLowerCase(),
        class: parent.className?.toString().slice(0, 60) || null,
        overflow: `${parentStyle.overflowX}/${parentStyle.overflowY}`,
        ringOutsideBoxPx: Object.fromEntries(Object.entries(out).map(([side, value]) => [side, Math.round(value * 10) / 10])),
      });
    }
  }
  return {
    element: {
      tag: element.tagName.toLowerCase(),
      role: element.getAttribute("role"),
      slot: element.getAttribute("data-slot"),
      label: (element.getAttribute("aria-label") ?? element.textContent ?? "").replace(/\s+/g, " ").trim().slice(0, 40),
    },
    focusVisible: element.matches(":focus-visible"),
    outlineStyle: style.outlineStyle,
    outlineWidth: style.outlineWidth,
    outlineColor: style.outlineColor,
    outlineOffset: style.outlineOffset,
    twOutlineStyle: style.getPropertyValue("--tw-outline-style"),
    ringColorToken: getComputedStyle(document.documentElement).getPropertyValue("--ring").trim(),
    rect: { x: Math.round(rect.x), y: Math.round(rect.y), width: Math.round(rect.width), height: Math.round(rect.height) },
    clippedBy,
  };
}

async function shoot(page, scheme, name) {
  const box = await page.evaluate(() => {
    const element = document.activeElement;
    if (!element) return null;
    const rect = element.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  });
  const pad = 32;
  const clip =
    box === null
      ? undefined
      : {
          x: Math.max(0, box.x - pad),
          y: Math.max(0, box.y - pad),
          width: Math.min(1280, Math.min(box.width, 700) + pad * 2),
          height: Math.min(720, Math.min(box.height, 300) + pad * 2),
        };
  const image = await page.screenshot({ type: "jpeg", quality: 70, scale: "css", clip });
  console.log(`SHOT ${MODE}--${scheme}--${name} ${image.toString("base64")}`);
}

/** Presses Tab from the top of the page until the focused element matches `selector`; null when it never does. */
async function tabTo(page, selector, from = null) {
  if (from === null) {
    await page.evaluate(() => {
      document.activeElement?.blur?.();
      window.scrollTo(0, 0);
    });
  }
  for (let presses = 1; presses <= TAB_LIMIT; presses += 1) {
    await page.keyboard.press("Tab");
    const matches = await page.evaluate((css) => document.activeElement?.matches(css) ?? false, selector);
    if (matches) return presses;
  }
  return null;
}

async function gotoReady(page, path, readySelector, what) {
  await page.goto(`${BASE}${path}`, { waitUntil: "domcontentloaded" });
  try {
    await page.locator(readySelector).first().waitFor({ state: "visible", timeout: WAIT_MS });
  } catch {
    throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for ${what} on ${path}`);
  }
}

/** Two animation frames: the reduced-motion rule leaves a 0.01 ms transition, so a read in the same frame sees the start values. */
async function settle(page) {
  await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
  await page.waitForTimeout(200);
}

async function tabStep(page, scheme, step, selector) {
  const presses = await tabTo(page, selector);
  if (presses === null) {
    failed = true;
    emit(scheme, step, { FAILED: `Tab never reached ${selector} within ${TAB_LIMIT} presses` });
    return;
  }
  await settle(page);
  emit(scheme, step, { tabPresses: presses, ...(await page.evaluate(describeFocused)) });
  await shoot(page, scheme, step.replaceAll(" ", "-"));
}

for (const scheme of SCHEMES) {
  const context = await browser.newContext({
    viewport: { width: 1280, height: 720 },
    locale: "zh-TW",
    timezoneId: "Asia/Taipei",
    colorScheme: scheme,
    reducedMotion: "reduce",
  });
  const page = await context.newPage();
  const consoleErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text().slice(0, 200));
  });
  try {
    // The devices list: the skip link (first Tab stop), a button, and a link.
    await gotoReady(page, "/devices", "main", "the devices page");
    await tabStep(page, scheme, "tab to the skip link", "a[href='#main']");
    await tabStep(page, scheme, "tab to a button", "button[data-slot='button']");

    // A text input.
    await gotoReady(page, "/devices/new", "input[data-slot='input']", "the new-device form");
    await tabStep(page, scheme, "tab to a text input", "input[data-slot='input']");
    // A mouse click into a text input matches :focus-visible too (design canvas: the ring shows).
    await page.locator("input[data-slot='input']").first().click();
    await settle(page);
    emit(scheme, "mouse click into a text input", await page.evaluate(describeFocused));

    // The device page: the range toggle (HistoryCard repeated the classes inline) and the history chart (role="slider").
    await gotoReady(page, "/devices", "a[href^='/devices/']", "a device link");
    const href = await page.evaluate(() =>
      [...document.querySelectorAll("a[href^='/devices/']")]
        .map((link) => link.getAttribute("href"))
        .find((value) => value && !/\/(new|settings)$/.test(value)),
    );
    if (!href) throw new Error("the devices page lists no device link");
    await gotoReady(page, href, "fieldset button[aria-pressed]", "the range toggle");
    await tabStep(page, scheme, "tab to the range toggle", "fieldset button[aria-pressed]");
    // The chart is the next stop that is a slider; wait for it to draw first.
    await page.locator("[role='slider']").first().waitFor({ state: "visible", timeout: WAIT_MS });
    const chartPresses = await tabTo(page, "[role='slider']", "continue");
    if (chartPresses === null) throw new Error("Tab never reached the history chart");
    await settle(page);
    emit(scheme, "tab to a chart", { tabPresses: chartPresses, ...(await page.evaluate(describeFocused)) });
    await shoot(page, scheme, "tab-to-a-chart");

    // A mouse click on a button must not show the ring.
    const toggle = page.locator("fieldset button[aria-pressed]").nth(1);
    await toggle.click();
    await settle(page);
    emit(scheme, "mouse click on the range toggle", await page.evaluate(describeFocused));
    await shoot(page, scheme, "mouse-click-on-the-range-toggle");
  } catch (error) {
    failed = true;
    emit(scheme, "error", { FAILED: String(error?.message ?? error).slice(0, 300) });
  }
  emit(scheme, "console errors", { consoleErrors });
  await context.close();
}

await browser.close();
process.exit(failed ? 1 : 0);
