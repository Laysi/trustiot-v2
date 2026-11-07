// The two-column walkthrough of the demo script's steps 3 and 4 (Laysi/trustiot-v2#1195), run in
// the Playwright image (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`).
// It opens the preview conversation whose answers wrote a dashboard draft (two turns: version 1,
// then the follow-up's version 2) in Traditional Chinese (zh-TW, Asia/Taipei, light theme, zoom
// 100 %) at CSS viewports of 1280×720, 1536×768 and 1920×1080, which are two columns from the
// `xl` breakpoint (1280 px), and at 1279×720 as the one-column control. Every step prints one JSON
// line of DOM measurements: both columns' widths and places, where the question, the answers,
// their dashboard cards and charts and the draft panel are against the first screen, horizontal
// overflow, clipped or spilling text, the sticky bars; then what saving from the last answer's
// card does in the answer and in the panel; then the source panel opened from a KPI number in the
// answer's card and in the panel. Screenshots (JPEG) are printed base64 after the JSON lines,
// between markers, for the reviewer's eyes; they are never part of the committed record. What only
// the preview draws (its toolbar, the composer's completion hint and template button) is hidden,
// so the numbers are the product's layout (see previewEmulation); unlike the #1179 walkthrough,
// the draft-list alert is NOT hidden: the preview answers the conversation's drafts now, so it
// must not appear, and the record says whether it does.
//
// Usage: docs/test-reports/data/2026-11-07-1195-two-column-walkthrough/run.sh (from the repository
// root). VIEWPORTS=<id,id> limits the run to some viewports; SHOTS=0 takes no screenshots;
// PREVIEW_AS_IS=1 keeps the preview-only parts.
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
const PREVIEW_CONVERSATION = "/conversations/3e7b1c94-8a52-4d60-b9f3-0c6d2a5e7f18";
/** What the preview conversation holds (src/test/fake-conversations.ts, fake-dashboards.ts). */
const QUESTION_1 = "How much energy did line 2 use this week?";
const QUESTION_2 = "Make it hourly and show last week's energy next to it.";
const DRAFT_TITLE = "Line 2 power draft";
const V1_FIRST = "Power by line";
const V1_CHART = "Daily average power";
const V2_KPI = "Energy, week of Oct 26";
const V2_CHART = "Hourly average power on Wednesday";

/** CSS viewport sizes at zoom 100 % in full screen; `xl` (1280 px) is where the page turns two columns. */
const ALL = [
  { id: "1280x720", width: 1280, height: 720, dsf: 1, scheme: "light" },
  { id: "1536x768", width: 1536, height: 768, dsf: 1, scheme: "light" },
  { id: "1920x1080", width: 1920, height: 1080, dsf: 1, scheme: "light" },
  { id: "1279x720-control", width: 1279, height: 720, dsf: 1, scheme: "light" },
];
const only = (process.env.VIEWPORTS ?? "").split(",").filter(Boolean);
const VIEWPORTS = only.length > 0 ? ALL.filter((viewport) => only.includes(viewport.id)) : ALL;
const TAKE_SHOTS = process.env.SHOTS !== "0";
/** Hide what only the preview draws (previewEmulation); PREVIEW_AS_IS=1 measures the preview as it is. */
const EMULATE = process.env.PREVIEW_AS_IS !== "1";

const browser = await chromium.launch();
const failures = [];
const consoleErrors = [];
const shots = [];

function emit(viewport, step, data) {
  const where = viewport
    ? { viewport: viewport.id, css: `${viewport.width}x${viewport.height}`, dsf: viewport.dsf, scheme: viewport.scheme }
    : {};
  console.log(JSON.stringify({ step, ...where, ...data, consoleErrors: [...consoleErrors] }));
  consoleErrors.length = 0;
}

/** Waits for `locator` to be visible, failing with a message that names what was awaited. */
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

/**
 * The page's horizontal overflow, the elements that leave the window, clipped text, the sticky and
 * fixed bars, and, for `key` (an element handle or null), where it is, how much of it shows between
 * the bars, and how its hidden part is reached. Runs in the page.
 */
async function measure(page, key, options = {}) {
  return page.evaluate(
    ([key, options]) => {
      const vw = document.documentElement.clientWidth;
      const vh = window.innerHeight;
      const text = (node) => node?.textContent?.replace(/\s+/g, " ").trim() ?? "";
      const describe = (element) => {
        if (!element) return null;
        const label =
          element.getAttribute("aria-label") ??
          element.getAttribute("data-testid") ??
          (element.id ? `#${element.id}` : null);
        return {
          tag: element.tagName.toLowerCase(),
          label,
          class: element.className?.toString().slice(0, 80) || null,
          text: text(element).slice(0, 50),
        };
      };
      const visible = (element) => {
        const style = getComputedStyle(element);
        if (style.display === "none" || style.visibility === "hidden") return false;
        // Visually hidden for assistive technology (Tailwind's sr-only clips to nothing).
        if (element.closest(".sr-only") || style.clipPath === "inset(50%)") return false;
        const rect = element.getBoundingClientRect();
        if (rect.width <= 1 && rect.height <= 1) return false;
        return true;
      };
      const all = [...document.querySelectorAll("#root *")].filter(visible);

      // Horizontal: elements whose box leaves the window. Those inside a clipping or scrolling
      // ancestor that itself fits are listed apart (a table wrapper meant to scroll sideways).
      const leaving = [];
      const contained = new Map();
      for (const element of all) {
        const rect = element.getBoundingClientRect();
        if (rect.right <= vw + 0.5 && rect.left >= -0.5) continue;
        let clipper = null;
        for (let up = element.parentElement; up && up.id !== "root"; up = up.parentElement) {
          const overflowX = getComputedStyle(up).overflowX;
          if (overflowX !== "visible") {
            clipper = up;
            break;
          }
        }
        const clipperRect = clipper?.getBoundingClientRect();
        if (clipper && clipperRect.right <= vw + 0.5 && clipperRect.left >= -0.5) {
          const entry = contained.get(clipper) ?? { container: describe(clipper), overflowX: getComputedStyle(clipper).overflowX, scrollable: clipper.scrollWidth > clipper.clientWidth, count: 0 };
          entry.count += 1;
          contained.set(clipper, entry);
          continue;
        }
        leaving.push({ left: Math.round(rect.left), right: Math.round(rect.right), ...describe(element) });
      }

      // Text cut by its own box: ellipsis, line clamp, or hidden overflow with text in it.
      const truncated = [];
      for (const element of all) {
        const style = getComputedStyle(element);
        const ownText = [...element.childNodes].some((node) => node.nodeType === 3 && node.textContent.trim() !== "");
        if (!ownText) continue;
        const clamps = style.webkitLineClamp !== "none" && style.webkitLineClamp !== "";
        const clips = style.textOverflow === "ellipsis" || clamps || ["hidden", "clip"].includes(style.overflowX);
        if (!clips) continue;
        const wide = element.scrollWidth > element.clientWidth + 1;
        const tall = clamps && element.scrollHeight > element.clientHeight + 1;
        if (wide || tall) {
          const rect = element.getBoundingClientRect();
          truncated.push({
            ...describe(element),
            fullText: text(element).slice(0, 120),
            clientWidth: element.clientWidth,
            scrollWidth: element.scrollWidth,
            onScreen: rect.bottom > 0 && rect.top < vh,
          });
        }
      }

      // Sticky and fixed bars, where they are now.
      const bars = [];
      for (const element of all) {
        const position = getComputedStyle(element).position;
        if (position !== "sticky" && position !== "fixed") continue;
        // A sticky table head scrolls with its table; only page-level bars matter here.
        if (element.closest("table")) continue;
        const rect = element.getBoundingClientRect();
        bars.push({ position, top: Math.round(rect.top), bottom: Math.round(rect.bottom), height: Math.round(rect.height), ...describe(element) });
      }
      const header = document.querySelector("#root header.sticky");
      const headerHeight = header ? Math.round(header.getBoundingClientRect().height) : 0;
      // The composer of a conversation is sticky at the bottom; it covers the window's bottom only
      // while it is stuck there.
      const bottomBar = bars.find((bar) => bar.position === "sticky" && bar.tag !== "header" && bar.bottom >= vh - 1 && bar.top > vh / 3);
      const bottomHeight = bottomBar ? bottomBar.height : 0;
      const band = { top: headerHeight, bottom: vh - bottomHeight, height: vh - headerHeight - bottomHeight };

      const scroller = document.scrollingElement;
      const result = {
        window: { width: window.innerWidth, height: vh, clientWidth: vw },
        lang: document.documentElement.lang,
        theme: document.documentElement.dataset.theme ?? null,
        page: { scrollWidth: scroller.scrollWidth, scrollHeight: scroller.scrollHeight, scrollY: Math.round(window.scrollY) },
        root: { scrollWidth: document.getElementById("root").scrollWidth, clientWidth: document.getElementById("root").clientWidth },
        horizontalScroll: scroller.scrollWidth > vw || document.getElementById("root").scrollWidth > document.getElementById("root").clientWidth,
        leaving: leaving.slice(0, 10),
        leavingCount: leaving.length,
        sidewaysContainers: [...contained.values()],
        truncated: truncated.slice(0, 12),
        truncatedCount: truncated.length,
        bars,
        band,
        previewHidden: window.__walkthroughHidden?.() ?? [],
      };

      if (key) {
        const describeKey = (rect) => {
          const visibleTop = Math.max(rect.top, band.top);
          const visibleBottom = Math.min(rect.bottom, band.bottom);
          const shown = Math.max(0, visibleBottom - visibleTop);
          return {
            top: Math.round(rect.top),
            bottom: Math.round(rect.bottom),
            height: Math.round(rect.height),
            width: Math.round(rect.width),
            shown: Math.round(shown),
            cut: Math.round(rect.height - shown),
          };
        };
        const atArrival = describeKey(key.getBoundingClientRect());
        // How the hidden part is reached: its own scroll, an ancestor's scroll, the page, or not at all.
        let reach = "fits";
        let ownScroll = key.scrollHeight > key.clientHeight + 1 && ["auto", "scroll"].includes(getComputedStyle(key).overflowY);
        let clippedBy = null;
        for (let up = key.parentElement; up && up !== document.body; up = up.parentElement) {
          const overflowY = getComputedStyle(up).overflowY;
          if (overflowY === "visible") continue;
          const upRect = up.getBoundingClientRect();
          const keyRect = key.getBoundingClientRect();
          if (keyRect.bottom > upRect.bottom + 1 || keyRect.top < upRect.top - 1) {
            clippedBy = { ...describe(up), overflowY };
            break;
          }
        }
        const height = key.getBoundingClientRect().height;
        if (height > band.height) {
          if (ownScroll) reach = "own scroll";
          else if (clippedBy && ["auto", "scroll"].includes(clippedBy.overflowY)) reach = "ancestor scroll";
          else if (clippedBy) reach = "not at all (clipped)";
          else reach = "page scroll";
        }
        // Bring its top just under the header, as a presenter scrolling to it would, and look again.
        if (!options.noScroll) {
          const rect = key.getBoundingClientRect();
          window.scrollBy(0, rect.top - band.top - 8);
        }
        const scrolled = describeKey(key.getBoundingClientRect());
        result.key = {
          ...describe(key),
          atArrival,
          scrolledToTop: scrolled,
          fitsBetweenBars: height <= band.height,
          cutWhenScrolled: scrolled.cut,
          reach,
          clippedBy,
          pageScrollY: Math.round(window.scrollY),
        };
      }

      const active = document.activeElement;
      if (active && active !== document.body) {
        const rect = active.getBoundingClientRect();
        result.focus = {
          ...describe(active),
          top: Math.round(rect.top),
          bottom: Math.round(rect.bottom),
          inBand: rect.top >= band.top - 1 && rect.bottom <= band.bottom + 1,
        };
      }
      return result;
    },
    [key, options],
  );
}

/**
 * Whether the stretch from the top of `first` to the bottom of `last` (for example a card's title
 * to its chart's bottom) can be on screen at once between the sticky bars, and how much is cut
 * when its top is scrolled just under the header.
 */
async function measureSpan(page, first, last) {
  return page.evaluate(
    ([first, last]) => {
      const vh = window.innerHeight;
      const header = document.querySelector("#root header.sticky");
      const top = header ? header.getBoundingClientRect().height : 0;
      // Scroll the stretch's top just under the header first: the conversation's composer sticks
      // to the bottom only while the page is longer than the window.
      window.scrollBy(0, first.getBoundingClientRect().top - top - 8);
      const composer = [...document.querySelectorAll("#root .sticky")].find((element) => {
        const rect = element.getBoundingClientRect();
        return element.tagName !== "HEADER" && !element.closest("table") && rect.bottom >= vh - 1 && rect.top > vh / 3;
      });
      const bottom = vh - (composer ? composer.getBoundingClientRect().height : 0);
      const from = first.getBoundingClientRect().top;
      const to = last.getBoundingClientRect().bottom;
      const shown = Math.max(0, Math.min(to, bottom) - Math.max(from, top));
      return {
        height: Math.round(to - from),
        band: Math.round(bottom - top),
        fits: to - from <= bottom - top,
        cut: Math.round(to - from - shown),
      };
    },
    [first, last],
  );
}

/** Where `locator`'s element is against the window and the sticky bars, without scrolling. */
async function inWindow(locator) {
  return locator.first().evaluate((element) => {
    const vh = window.innerHeight;
    const header = document.querySelector("#root header.sticky");
    const top = header ? header.getBoundingClientRect().height : 0;
    const composer = [...document.querySelectorAll("#root .sticky")].find((candidate) => {
      const rect = candidate.getBoundingClientRect();
      return candidate.tagName !== "HEADER" && !candidate.closest("table") && rect.bottom >= vh - 1 && rect.top > vh / 3;
    });
    const bottom = vh - (composer && !composer.contains(element) ? composer.getBoundingClientRect().height : 0);
    const rect = element.getBoundingClientRect();
    return {
      top: Math.round(rect.top),
      bottom: Math.round(rect.bottom),
      visible: rect.top >= top - 1 && rect.bottom <= bottom + 1,
      right: Math.round(rect.right),
    };
  });
}

/**
 * Runs in every page before the app: hides what the UI preview draws and the product does not, so
 * the measurements are the product's layout (set PREVIEW_AS_IS=1 to measure the preview as it is).
 * - the preview's toolbar above #root;
 * - the composer's grey-completion hint and 「問句範本」 button: completion and templates exist only
 *   in the preview (their providers are pending APIs).
 * The conversation page's 「讀不到這段對話的草稿」 alert is deliberately NOT hidden (see the header).
 * `window.__walkthroughHidden` lists what was hidden.
 */
function previewEmulation(emulate) {
  const hidden = new Set();
  const apply = () => {
    if (!emulate) return;
    for (const box of document.querySelectorAll("textarea[aria-describedby]")) {
      for (const id of box.getAttribute("aria-describedby").split(/\s+/)) {
        const hint = document.getElementById(id);
        if (!hint) continue;
        for (const node of hint.childNodes) {
          if (node.nodeType === 3 && node.nodeValue.includes("灰字")) {
            node.nodeValue = "";
            hidden.add("completion-hint");
          }
        }
      }
    }
  };
  window.__walkthroughHidden = () => [...hidden];
  document.addEventListener("DOMContentLoaded", () => {
    const style = document.createElement("style");
    style.textContent = [
      "#ui-preview-toolbar{display:none!important}",
      ...(emulate ? ['button[aria-label="問句範本（/）"]{display:none!important}'] : []),
    ].join("\n");
    document.head.append(style);
    new MutationObserver(apply).observe(document.body, { subtree: true, childList: true, characterData: true });
    apply();
  });
}

/**
 * Where each target (name to element handle, or null) is against the window and the sticky bars,
 * at the current scroll: box in the viewport and in the document, how much of it shows between the
 * header and the composer (which only covers what it overlaps sideways), whether all of it does,
 * and whether it starts above the composer.
 */
async function places(page, targets) {
  const names = Object.keys(targets);
  return page.evaluate(
    ([names, handles]) => {
      const vh = window.innerHeight;
      const header = document.querySelector("#root header.sticky");
      const headerBottom = header ? header.getBoundingClientRect().bottom : 0;
      const composer = [...document.querySelectorAll("#root .sticky")].find((candidate) => {
        const rect = candidate.getBoundingClientRect();
        return candidate.tagName !== "HEADER" && !candidate.closest("table") && rect.bottom >= vh - 1 && rect.top > vh / 3;
      });
      const composerRect = composer?.getBoundingClientRect();
      const out = {};
      names.forEach((name, index) => {
        const element = handles[index];
        if (!element) {
          out[name] = null;
          return;
        }
        const rect = element.getBoundingClientRect();
        const covered = composerRect && !composer.contains(element) && rect.left < composerRect.right && rect.right > composerRect.left;
        const limit = covered ? composerRect.top : vh;
        const shown = Math.max(0, Math.min(rect.bottom, limit) - Math.max(rect.top, headerBottom));
        out[name] = {
          left: Math.round(rect.left),
          right: Math.round(rect.right),
          width: Math.round(rect.width),
          top: Math.round(rect.top),
          bottom: Math.round(rect.bottom),
          height: Math.round(rect.height),
          docTop: Math.round(rect.top + window.scrollY),
          shown: Math.round(shown),
          cut: Math.round(rect.height - shown),
          inFirstScreen: rect.top >= headerBottom - 1 && rect.bottom <= limit + 1,
          startsInFirstScreen: rect.top >= headerBottom - 1 && rect.top < limit,
        };
      });
      out.__bars = { headerBottom: Math.round(headerBottom), composerTop: composerRect ? Math.round(composerRect.top) : null, composerLeft: composerRect ? Math.round(composerRect.left) : null, composerRight: composerRect ? Math.round(composerRect.right) : null, viewportHeight: vh };
      return out;
    },
    [names, Object.values(targets)],
  );
}

/** Text that spills out of its box: own text, no clipping, and the content is wider than the box. */
async function spilling(page) {
  return page.evaluate(() => {
    const out = [];
    for (const element of document.querySelectorAll("#root *")) {
      const style = getComputedStyle(element);
      if (style.display === "none" || style.display === "inline" || style.visibility === "hidden") continue;
      if (element.closest(".sr-only")) continue;
      const ownText = [...element.childNodes].some((node) => node.nodeType === 3 && node.textContent.trim() !== "");
      if (!ownText || style.overflowX !== "visible") continue;
      if (element.scrollWidth > element.clientWidth + 1) {
        out.push({
          tag: element.tagName.toLowerCase(),
          class: element.className?.toString().slice(0, 60) || null,
          text: element.textContent.replace(/\s+/g, " ").trim().slice(0, 60),
          clientWidth: element.clientWidth,
          scrollWidth: element.scrollWidth,
        });
      }
    }
    return { count: out.length, items: out.slice(0, 12) };
  });
}

async function walk(viewport) {
  const context = await browser.newContext({
    locale: "zh-TW",
    timezoneId: "Asia/Taipei",
    viewport: { width: viewport.width, height: viewport.height },
    deviceScaleFactor: viewport.dsf,
    colorScheme: viewport.scheme,
  });
  await context.addInitScript(previewEmulation, EMULATE);
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
      emit(viewport, `${name} FAILED`, { error: error.message.split("\n")[0], url: new URL(page.url()).pathname });
    }
  };
  const scrollTop = () => page.evaluate(() => window.scrollTo(0, 0));
  const scrollUnderHeader = (locator) =>
    locator.evaluate((element) => {
      const header = document.querySelector("#root header.sticky");
      window.scrollBy(0, element.getBoundingClientRect().top - (header?.getBoundingClientRect().height ?? 0) - 8);
    });
  const scrollY = () => page.evaluate(() => Math.round(window.scrollY));

  const answers = main.getByRole("article", { name: "TrustIoT 的回答" });
  const cards = main.locator("section[data-answer-dashboard]");
  const panelTitle = main.getByRole("heading", { level: 2, name: DRAFT_TITLE });
  const panel = panelTitle.locator("xpath=ancestor::div[contains(@class,'bg-muted')][1]");
  const panelHeader = panelTitle.locator("xpath=ancestor::div[contains(@class,'bg-card')][1]");
  const card1 = cards.nth(0);
  const card2 = cards.nth(1);
  const component = (root, title) => root.getByRole("heading", { level: 3, name: title }).locator("xpath=ancestor::section[1]");
  const plotOf = (root, title) =>
    component(root, title).locator("canvas").first().evaluateHandle((element) => element.closest(".h-72") ?? element.parentElement);
  const handle = (locator) => locator.first().elementHandle({ timeout: 10_000 });
  const SAVED = /已儲存為儀表板列表裡的第 \d+ 版/;

  // The state of one place that shows the draft: the answer's card or the panel.
  const stateOf = async (root, heading) => ({
    header: await heading.evaluate((element) => element.parentElement.textContent.replace(/\s+/g, " ").trim()),
    saveButtons: await root.getByRole("button", { name: "儲存到儀表板列表" }).count(),
    notice: await root.getByText(SAVED).count(),
    openLink: await root.getByRole("link", { name: "開啟儀表板" }).count(),
  });

  /**
   * Opens the source panel from the KPI number of `root` (the answer's card or the draft panel):
   * the component's top is scrolled just under the header first, as a presenter who shows the
   * number and then clicks it has it. Records where the panel opens (column, box, how much of it
   * shows without scrolling) and where it is with its own top scrolled under the header.
   */
  const openSource = async (root, where, shotName) => {
    const kpi = component(root, V2_KPI);
    await waitVisible(kpi, `the KPI ${V2_KPI} in ${where}`);
    await scrollUnderHeader(kpi);
    const scrollBefore = await scrollY();
    const button = kpi.getByRole("button", { name: /查看來源$/ }).first();
    await waitVisible(button, `the KPI value's 查看來源 button in ${where}`);
    const buttonPlace = (await places(page, { button: await button.elementHandle() })).button;
    await button.click();
    const source = kpi.getByRole("region", { name: "資料來源" });
    await waitVisible(source, `the source panel in ${where}`);
    await waitVisible(source.getByText("筆數", { exact: true }), `the source panel's 筆數 line in ${where}`);
    const sourceHandle = await source.elementHandle();
    const box = await source.boundingBox();
    emit(viewport, `${where}: source panel as opened`, {
      scrollBefore,
      scrollAfter: await scrollY(),
      buttonPlace,
      box: box && { x: Math.round(box.x), width: Math.round(box.width) },
      ...(await measure(page, sourceHandle, { noScroll: true })),
    });
    await shoot(page, viewport, `${shotName}`);
    const scrolled = await measure(page, sourceHandle);
    emit(viewport, `${where}: source panel scrolled to its top`, scrolled);
    await shoot(page, viewport, `${shotName}-top`);
    await source.getByRole("button", { name: "關閉來源" }).click();
    await source.waitFor({ state: "hidden", timeout: WAIT_MS });
  };

  await step("open", async () => {
    await page.goto(`${BASE}${PREVIEW_CONVERSATION}`);
    await waitVisible(answers.first(), "the preview conversation's first answer");
    await page.waitForFunction(() => document.querySelectorAll("#root main section[data-answer-dashboard]").length === 2, null, { timeout: WAIT_MS }).catch(() => {
      throw new Error("timed out after 60 s waiting for the two answers' dashboard cards");
    });
    await waitVisible(panelTitle, `the draft panel's title ${DRAFT_TITLE}`);
    // The answers' and the panel's datasets run once they are near the window: bring each near,
    // wait for its chart and KPI, then return to the top, so every box is measured settled.
    for (const [root, title, what] of [
      [card1, V1_FIRST, "the first answer's first chart"],
      [card1, V1_CHART, "the first answer's second chart"],
      [card2, V2_CHART, "the second answer's chart"],
      [panel, V2_CHART, "the panel's chart"],
    ]) {
      await component(root, title).scrollIntoViewIfNeeded();
      await waitVisible(component(root, title).locator("canvas").first(), what);
    }
    for (const [root, what] of [
      [card2, "the second answer's KPI value"],
      [panel, "the panel's KPI value"],
    ]) {
      await component(root, V2_KPI).scrollIntoViewIfNeeded();
      await waitVisible(component(root, V2_KPI).getByRole("button", { name: /查看來源$/ }).first(), what);
    }
    await scrollTop();
    // The page settles (fonts, chart sizes) before the boxes are read.
    await page.evaluate(() => document.fonts.ready);

    const leftColumn = await main.locator("h1").first().evaluateHandle((element) => element.closest("[class*=max-w-3xl]"));
    const wanted = {
      main: () => main.elementHandle({ timeout: 10_000 }),
      grid: () => leftColumn.evaluateHandle((element) => element.parentElement),
      leftColumn: () => leftColumn,
      rightColumn: () => handle(panel),
      title: () => handle(main.locator("h1")),
      question1: () => handle(main.getByText(QUESTION_1)),
      answer1: () => handle(answers.nth(0)),
      card1: () => handle(card1),
      card1Header: () => handle(card1.locator("h3")),
      card1Chart: () => plotOf(card1, V1_FIRST),
      question2: () => handle(main.getByText(QUESTION_2)),
      answer2: () => handle(answers.nth(1)),
      card2: () => handle(card2),
      card2Header: () => handle(card2.locator("h3")),
      card2Kpi: () => handle(component(card2, V2_KPI)),
      card2Chart: () => plotOf(card2, V2_CHART),
      panelHeader: () => handle(panelHeader),
      panelKpi: () => handle(component(panel, V2_KPI)),
      panelChart: () => plotOf(panel, V2_CHART),
    };
    const targets = {};
    for (const [name, find] of Object.entries(wanted)) {
      try {
        targets[name] = await find();
      } catch {
        throw new Error(`the target ${name} was not found`);
      }
    }
    const found = await places(page, targets);
    const left = found.leftColumn;
    const right = found.rightColumn;
    emit(viewport, "layout at the top of the page", {
      columns: {
        sideBySide: right.left >= left.right - 1 && Math.abs(right.top - left.top) < 40,
        leftWidth: left.width,
        rightWidth: right.width,
        gap: right.left - left.right,
        mainWidth: found.main.width,
        gridWidth: found.grid.width,
        leftX: left.left,
        rightX: right.left,
      },
      draftsAlert: await main.getByText(/^讀不到這段對話的草稿/).count(),
      places: found,
      spilling: await spilling(page),
      ...(await measure(page, null)),
    });
    await shoot(page, viewport, "arrival");
  });

  await step("second answer", async () => {
    await scrollUnderHeader(card2);
    const view = await measure(page, await handle(card2), { noScroll: true });
    const at = await places(page, {
      answer2: await handle(answers.nth(1)),
      card2: await handle(card2),
      card2Header: await handle(card2.locator("h3")),
      card2Chart: await plotOf(card2, V2_CHART),
      panelHeader: await handle(panelHeader),
      panelChart: await plotOf(panel, V2_CHART),
    });
    emit(viewport, "second answer: its card's top scrolled under the header", { scrollY: await scrollY(), places: at, ...view });
    await shoot(page, viewport, "second-answer");
  });

  await step("source from the answer", async () => {
    await openSource(card2, "answer (left column)", "source-answer");
  });

  await step("source from the panel", async () => {
    await openSource(panel, "panel", "source-panel");
  });

  // Saving from the LAST answer's card (the runbook: only that one), then both places.
  await step("save", async () => {
    await scrollTop();
    const before = {
      card1: await stateOf(card1, card1.locator("h3").first()),
      card2: await stateOf(card2, card2.locator("h3").first()),
      panel: await stateOf(panel, panelTitle),
    };
    await scrollUnderHeader(card2);
    const save = card2.getByRole("button", { name: "儲存到儀表板列表" });
    await waitVisible(save, "the last answer's 儲存到儀表板列表 button");
    const savePlace = (await places(page, { save: await save.elementHandle() })).save;
    await save.click();
    await waitVisible(card2.getByText(SAVED), "the saved notice in the last answer's card");
    const panelSaved = await panel.getByText(SAVED).first().waitFor({ state: "visible", timeout: 15_000 }).then(() => true, () => false);
    const noticeTargets = async () => ({
      card2Notice: await handle(card2.getByText(SAVED)),
      card2Link: await handle(card2.getByRole("link", { name: "開啟儀表板" })),
      card2Header: await handle(card2.locator("h3")),
      panelNotice: panelSaved ? await handle(panel.getByText(SAVED)) : null,
      panelLink: panelSaved ? await handle(panel.getByRole("link", { name: "開啟儀表板" })) : null,
      panelHeader: await handle(panelHeader),
    });
    emit(viewport, "saved: both places while the last answer's card is in view", {
      scrollY: await scrollY(),
      savePlace,
      before,
      after: {
        card1: await stateOf(card1, card1.locator("h3").first()),
        card2: await stateOf(card2, card2.locator("h3").first()),
        panel: await stateOf(panel, panelTitle),
      },
      panelShowsSaved: panelSaved,
      places: await places(page, await noticeTargets()),
    });
    await shoot(page, viewport, "saved");
    await scrollTop();
    emit(viewport, "saved: both places at the top of the page", {
      scrollY: await scrollY(),
      places: await places(page, await noticeTargets()),
    });
    await shoot(page, viewport, "saved-top");
  });

  await context.close();
}

for (const viewport of VIEWPORTS) {
  await walk(viewport);
}
await browser.close();
emit(null, "summary", { viewports: VIEWPORTS.map((viewport) => `${viewport.id} ${viewport.width}x${viewport.height}@${viewport.dsf} ${viewport.scheme}`), failures });

if (shots.length > 0) {
  console.log("=== SCREENSHOTS BEGIN ===");
  for (const shot of shots) console.log(`SHOT ${shot.name} ${shot.base64}`);
  console.log("=== SCREENSHOTS END ===");
}
// A step that did not finish makes the run fail, after the record and the screenshots are out.
process.exitCode = failures.length > 0 ? 1 : 0;
