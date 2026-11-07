// Measures one layout of the conversation page with the draft panel (Laysi/trustiot-v2#1196): today's
// layout or one of the prototypes in this directory (option-*.patch), applied by run.sh. Adapted from
// the Laysi/trustiot-v2#1195 walkthrough (../2026-11-07-1195-two-column-walkthrough/browser-check.mjs):
// the same preview conversation (two turns: version 1, then the follow-up's version 2), the same
// viewports (1280×720, 1536×768 and 1920×1080, and 1279×720 as the one-column control), zh-TW,
// Asia/Taipei, light, zoom 100 %, in the Playwright image against the UI preview. Every step prints
// one JSON line carrying `option` (env OPTION). Unlike #1195's script, it
//   - clips what it reports as shown by every scrolling or clipping ancestor (a panel that scrolls
//     on its own hides what is outside its box), not only by the sticky bars;
//   - scrolls the nearest scrolling ancestor, not the page, to bring something to the top (in a
//     panel that scrolls on its own, the presenter scrolls the panel);
//   - records the panel's status line while the latest answer is in view, whether both saved
//     states show at once, the source panel opened from the panel's chart too, the panel's own
//     overflow, what a mouse wheel over the panel does, and Tab from the composer into the panel;
//   - ends each viewport with one `summary row` line of the numbers the report's tables use.
// Screenshots (JPEG) are printed base64 after the JSON lines, between markers; they are never
// committed. At 1280×720 every step is shot; at the other viewports only the top of the page, the
// latest answer and the saved state.
//
// Usage: run.sh in this directory (from the repository root). OPTION names the layout in the
// record; VIEWPORTS=<id,id> limits the viewports; SHOTS=0 takes no screenshots.
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
const OPTION = process.env.OPTION ?? "baseline";
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

/** CSS viewport sizes at zoom 100 % in full screen; `xl` (1280 px) is where today's page turns two columns. */
const ALL = [
  { id: "1280x720", width: 1280, height: 720, dsf: 1, scheme: "light" },
  { id: "1536x768", width: 1536, height: 768, dsf: 1, scheme: "light" },
  { id: "1920x1080", width: 1920, height: 1080, dsf: 1, scheme: "light" },
  { id: "1279x720-control", width: 1279, height: 720, dsf: 1, scheme: "light" },
];
const only = (process.env.VIEWPORTS ?? "").split(",").filter(Boolean);
const unknown = only.filter((id) => !ALL.some((viewport) => viewport.id === id));
if (unknown.length > 0) {
  throw new Error(`unknown VIEWPORTS ${unknown.join(", ")}; known: ${ALL.map((viewport) => viewport.id).join(", ")}`);
}
const VIEWPORTS = only.length > 0 ? ALL.filter((viewport) => only.includes(viewport.id)) : ALL;
const TAKE_SHOTS = process.env.SHOTS !== "0";
/** Shots taken at every viewport; 1280×720 takes all of them. */
const EVERY_VIEWPORT_SHOTS = new Set(["arrival", "second-answer", "saved"]);

const browser = await chromium.launch();
const failures = [];
const consoleErrors = [];
const shots = [];

function emit(viewport, step, data) {
  const where = viewport
    ? { viewport: viewport.id, css: `${viewport.width}x${viewport.height}`, dsf: viewport.dsf, scheme: viewport.scheme }
    : {};
  console.log(JSON.stringify({ option: OPTION, step, ...where, ...data, consoleErrors: [...consoleErrors] }));
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
  if (viewport.id !== "1280x720" && !EVERY_VIEWPORT_SHOTS.has(name)) return;
  const image = await page.screenshot({ type: "jpeg", quality: 50, scale: "css" });
  shots.push({ name: `${OPTION}--${viewport.id}--${name}`, base64: image.toString("base64") });
}

/**
 * Waits until the page and every scroll container in it have stopped moving (the same positions
 * over five animation frames), so a wheel or a focus change is read after its scrolling. Bounded;
 * on the bound it returns `false` instead of failing, and the record says so.
 */
async function settle(page) {
  return page.evaluate(
    (limit) =>
      new Promise((resolve) => {
        const started = performance.now();
        const positions = () =>
          [window.scrollY, ...[...document.querySelectorAll("#root *")].filter((element) => element.scrollTop > 0).map((element) => element.scrollTop)].join(",");
        let last = positions();
        let still = 0;
        const tick = () => {
          const now = positions();
          still = now === last ? still + 1 : 0;
          last = now;
          if (still >= 5) resolve(true);
          else if (performance.now() - started > limit) resolve(false);
          else requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
      }),
    10_000,
  );
}

/**
 * The page's horizontal overflow, the elements that leave the window, clipped text, the sticky and
 * fixed bars, and, for `key` (an element handle or null), where it is, how much of it shows between
 * the bars, and how its hidden part is reached. Runs in the page. (From #1195's script.)
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
        if (element.closest(".sr-only") || style.clipPath === "inset(50%)") return false;
        const rect = element.getBoundingClientRect();
        if (rect.width <= 1 && rect.height <= 1) return false;
        return true;
      };
      const all = [...document.querySelectorAll("#root *")].filter(visible);

      const leaving = [];
      for (const element of all) {
        const rect = element.getBoundingClientRect();
        if (rect.right <= vw + 0.5 && rect.left >= -0.5) continue;
        let clipper = null;
        for (let up = element.parentElement; up && up.id !== "root"; up = up.parentElement) {
          if (getComputedStyle(up).overflowX !== "visible") {
            clipper = up;
            break;
          }
        }
        const clipperRect = clipper?.getBoundingClientRect();
        if (clipper && clipperRect.right <= vw + 0.5 && clipperRect.left >= -0.5) continue;
        leaving.push({ left: Math.round(rect.left), right: Math.round(rect.right), ...describe(element) });
      }

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
          truncated.push({ ...describe(element), clientWidth: element.clientWidth, scrollWidth: element.scrollWidth });
        }
      }

      const bars = [];
      for (const element of all) {
        const position = getComputedStyle(element).position;
        if (position !== "sticky" && position !== "fixed") continue;
        if (element.closest("table")) continue;
        const rect = element.getBoundingClientRect();
        bars.push({ position, top: Math.round(rect.top), bottom: Math.round(rect.bottom), height: Math.round(rect.height), ...describe(element) });
      }
      const header = document.querySelector("#root header.sticky");
      const headerHeight = header ? Math.round(header.getBoundingClientRect().height) : 0;
      const bottomBar = bars.find((bar) => bar.position === "sticky" && bar.tag !== "header" && bar.bottom >= vh - 1 && bar.top > vh / 3);
      const bottomHeight = bottomBar ? bottomBar.height : 0;
      const band = { top: headerHeight, bottom: vh - bottomHeight, height: vh - headerHeight - bottomHeight };

      const scroller = document.scrollingElement;
      const result = {
        window: { width: window.innerWidth, height: vh, clientWidth: vw },
        lang: document.documentElement.lang,
        page: { scrollWidth: scroller.scrollWidth, scrollHeight: scroller.scrollHeight, scrollY: Math.round(window.scrollY) },
        horizontalScroll: scroller.scrollWidth > vw || document.getElementById("root").scrollWidth > document.getElementById("root").clientWidth,
        leaving: leaving.slice(0, 10),
        leavingCount: leaving.length,
        truncated: truncated.slice(0, 12),
        truncatedCount: truncated.length,
        bars,
        band,
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
        let reach = "fits";
        const ownScroll = key.scrollHeight > key.clientHeight + 1 && ["auto", "scroll"].includes(getComputedStyle(key).overflowY);
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
        if (height > band.height || clippedBy) {
          if (ownScroll) reach = "own scroll";
          else if (clippedBy && ["auto", "scroll"].includes(clippedBy.overflowY)) reach = "ancestor scroll";
          else if (clippedBy) reach = "not at all (clipped)";
          else reach = "page scroll";
        }
        result.key = { ...describe(key), atArrival, fitsBetweenBars: height <= band.height, reach, clippedBy };
      }

      const active = document.activeElement;
      if (active && active !== document.body) {
        const rect = active.getBoundingClientRect();
        result.focus = { ...describe(active), top: Math.round(rect.top), bottom: Math.round(rect.bottom) };
      }
      return result;
    },
    [key, options],
  );
}

/**
 * Where each target (name to element handle, or null) is against the window, the sticky bars and
 * every ancestor that clips it (a panel that scrolls on its own), at the current scroll: box in the
 * viewport and in the document, how much of it shows (between the header and the composer, which
 * only covers what it overlaps sideways, and inside every clipping ancestor), whether all of it does.
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
        let top = Math.max(rect.top, headerBottom);
        let bottom = Math.min(rect.bottom, covered ? composerRect.top : vh);
        let clippedBy = null;
        for (let up = element.parentElement; up && up.id !== "root"; up = up.parentElement) {
          if (getComputedStyle(up).overflowY === "visible") continue;
          const upRect = up.getBoundingClientRect();
          if (upRect.top > top || upRect.bottom < bottom) {
            if (rect.top < upRect.top - 1 || rect.bottom > upRect.bottom + 1) clippedBy ??= up.className.toString().slice(0, 60);
            top = Math.max(top, upRect.top);
            bottom = Math.min(bottom, upRect.bottom);
          }
        }
        const shown = Math.max(0, bottom - top);
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
          whole: shown >= rect.height - 1,
          clippedBy,
        };
      });
      out.__bars = { headerBottom: Math.round(headerBottom), composerTop: composerRect ? Math.round(composerRect.top) : null, composerLeft: composerRect ? Math.round(composerRect.left) : null, viewportHeight: vh };
      return out;
    },
    [names, Object.values(targets)],
  );
}

/** The draft panel's own scrolling: its box, content height, overflow and scroll position. */
async function panelScroll(handle) {
  return handle.evaluate((element) => {
    const style = getComputedStyle(element);
    const rect = element.getBoundingClientRect();
    return {
      position: style.position,
      stickyTop: style.top,
      maxHeight: style.maxHeight,
      overflowY: style.overflowY,
      top: Math.round(rect.top),
      bottom: Math.round(rect.bottom),
      clientHeight: element.clientHeight,
      scrollHeight: element.scrollHeight,
      scrollTop: Math.round(element.scrollTop),
      scrolls: element.scrollHeight > element.clientHeight + 1 && ["auto", "scroll"].includes(style.overflowY),
      tabIndex: element.getAttribute("tabindex"),
      role: element.getAttribute("role"),
      label: element.getAttribute("aria-label"),
    };
  });
}

/**
 * Runs in every page before the app: hides what the UI preview draws and the product does not, as
 * #1195's script does (the preview toolbar, the composer's completion hint and 問句範本 button).
 */
function previewEmulation() {
  const apply = () => {
    for (const box of document.querySelectorAll("textarea[aria-describedby]")) {
      for (const id of box.getAttribute("aria-describedby").split(/\s+/)) {
        const hint = document.getElementById(id);
        if (!hint) continue;
        for (const node of hint.childNodes) {
          if (node.nodeType === 3 && node.nodeValue.includes("灰字")) node.nodeValue = "";
        }
      }
    }
  };
  document.addEventListener("DOMContentLoaded", () => {
    const style = document.createElement("style");
    style.textContent = ['#ui-preview-toolbar{display:none!important}', 'button[aria-label="問句範本（/）"]{display:none!important}'].join("\n");
    document.head.append(style);
    new MutationObserver(apply).observe(document.body, { subtree: true, childList: true, characterData: true });
    apply();
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
  await context.addInitScript(previewEmulation);
  const page = await context.newPage();
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text().slice(0, 300));
  });
  page.on("pageerror", (error) => consoleErrors.push(`pageerror: ${error.message.slice(0, 300)}`));
  const main = page.locator("#root main");
  const row = { option: OPTION, viewport: viewport.id };
  const step = async (name, body) => {
    try {
      await body();
    } catch (error) {
      failures.push(`${OPTION} ${viewport.id} ${name}: ${error.message.split("\n")[0]}`);
      emit(viewport, `${name} FAILED`, { error: error.message.split("\n")[0], url: new URL(page.url()).pathname });
    }
  };
  // The page and every scroll container in it back to their tops.
  const resetScroll = async () => {
    await page.evaluate(() => {
      window.scrollTo(0, 0);
      for (const element of document.querySelectorAll("#root *")) if (element.scrollTop > 0) element.scrollTop = 0;
    });
    await settle(page);
  };
  // Brings `locator`'s top 8 px under the top of what shows it: its nearest scroll container that
  // scrolls (a panel scrolling on its own), or else the page, just under the header.
  const scrollUnderHeader = async (locator) => {
    await locator.evaluate((element) => {
      for (let up = element.parentElement; up && up.id !== "root"; up = up.parentElement) {
        const style = getComputedStyle(up);
        if (["auto", "scroll"].includes(style.overflowY) && up.scrollHeight > up.clientHeight + 1) {
          up.scrollTop += element.getBoundingClientRect().top - up.getBoundingClientRect().top - 8;
          return;
        }
      }
      const header = document.querySelector("#root header.sticky");
      window.scrollBy(0, element.getBoundingClientRect().top - (header?.getBoundingClientRect().height ?? 0) - 8);
    });
    await settle(page);
  };
  const scrollY = () => page.evaluate(() => Math.round(window.scrollY));

  const answers = main.getByRole("article", { name: "TrustIoT 的回答" });
  const cards = main.locator("section[data-answer-dashboard]");
  const panelTitle = main.getByRole("heading", { level: 2, name: DRAFT_TITLE });
  const panel = panelTitle.locator("xpath=ancestor::div[contains(@class,'bg-muted')][1]");
  const panelHeader = panelTitle.locator("xpath=ancestor::div[contains(@class,'bg-card')][1]");
  const panelStatus = panelTitle.locator("xpath=following-sibling::*[1]");
  const card1 = cards.nth(0);
  const card2 = cards.nth(1);
  const card2Status = card2.locator("h3").first().locator("xpath=following-sibling::*[1]");
  const component = (root, title) => root.getByRole("heading", { level: 3, name: title }).locator("xpath=ancestor::section[1]");
  const plotOf = (root, title) =>
    component(root, title).locator("canvas").first().evaluateHandle((element) => element.closest(".h-72") ?? element.parentElement);
  const handle = (locator) => locator.first().elementHandle({ timeout: 10_000 });
  const SAVED = /已儲存為儀表板列表裡的第 \d+ 版/;

  const stateOf = async (root, heading) => ({
    header: await heading.evaluate((element) => element.parentElement.textContent.replace(/\s+/g, " ").trim()),
    saveButtons: await root.getByRole("button", { name: "儲存到儀表板列表" }).count(),
    notice: await root.getByText(SAVED).count(),
    openLink: await root.getByRole("link", { name: "開啟儀表板" }).count(),
  });

  /**
   * Opens the source panel of `title` in `root` (the answer's card or the draft panel) from its KPI
   * number (`kind` "kpi") or from the component's 資料來源 button (`kind` "chart"), after scrolling
   * the component's top just under the top of what shows it. Records where the source panel opens,
   * how much shows, how the rest is reached, and what scrolled.
   */
  const openSource = async (root, where, kind, shotName) => {
    const title = kind === "kpi" ? V2_KPI : V2_CHART;
    const host = component(root, title);
    await waitVisible(host, `${title} in ${where}`);
    await scrollUnderHeader(host);
    const panelHandle = await handle(panel);
    const before = { scrollY: await scrollY(), panelScrollTop: (await panelScroll(panelHandle)).scrollTop };
    const button =
      kind === "kpi"
        ? host.getByRole("button", { name: /查看來源$/ }).first()
        : host.getByRole("button", { name: `「${title}」的資料來源` });
    await waitVisible(button, `the ${kind} source button in ${where}`);
    await button.click();
    const source = host.getByRole("region", { name: "資料來源" });
    await waitVisible(source, `the source panel in ${where}`);
    await waitVisible(source.getByText("筆數", { exact: true }), `the source panel's 筆數 line in ${where}`);
    await settle(page);
    const sourceHandle = await source.elementHandle();
    const placed = (await places(page, { source: sourceHandle })).source;
    const measured = await measure(page, sourceHandle, { noScroll: true });
    const after = { scrollY: await scrollY(), panelScrollTop: (await panelScroll(panelHandle)).scrollTop };
    emit(viewport, `${where}: ${kind} source panel as opened`, { before, after, place: placed, reach: measured.key.reach, clippedBy: measured.key.clippedBy, truncated: measured.truncated, focus: measured.focus });
    await shoot(page, viewport, shotName);
    await source.getByRole("button", { name: "關閉來源" }).click();
    await source.waitFor({ state: "hidden", timeout: WAIT_MS });
    return { width: placed.width, height: placed.height, shown: placed.shown, reach: measured.key.reach };
  };

  await step("open", async () => {
    await page.goto(`${BASE}${PREVIEW_CONVERSATION}`);
    await waitVisible(answers.first(), "the preview conversation's first answer");
    await page.waitForFunction(() => document.querySelectorAll("#root main section[data-answer-dashboard]").length === 2, null, { timeout: WAIT_MS }).catch(() => {
      throw new Error("timed out after 60 s waiting for the two answers' dashboard cards");
    });
    await waitVisible(panelTitle, `the draft panel's title ${DRAFT_TITLE}`);
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
    await resetScroll();
    await page.evaluate(() => document.fonts.ready);

    const leftColumn = await main.locator("h1").first().evaluateHandle((element) => element.closest("[class*=max-w-3xl]"));
    const wanted = {
      main: () => main.elementHandle({ timeout: 10_000 }),
      grid: () => leftColumn.evaluateHandle((element) => element.parentElement),
      leftColumn: () => leftColumn,
      rightColumn: () => handle(panel),
      question1: () => handle(main.getByText(QUESTION_1)),
      card1: () => handle(card1),
      card1Chart: () => plotOf(card1, V1_FIRST),
      question2: () => handle(main.getByText(QUESTION_2)),
      answer2: () => handle(answers.nth(1)),
      card2: () => handle(card2),
      card2Header: () => handle(card2.locator("h3")),
      card2Kpi: () => handle(component(card2, V2_KPI)),
      card2Chart: () => plotOf(card2, V2_CHART),
      panelHeader: () => handle(panelHeader),
      panelStatus: () => handle(panelStatus),
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
    const columns = {
      sideBySide: right.left >= left.right - 1 && Math.abs(right.top - left.top) < 40,
      leftWidth: left.width,
      rightWidth: right.width,
      gap: right.left - left.right,
      mainWidth: found.main.width,
      gridWidth: found.grid.width,
      leftX: left.left,
      rightX: right.left,
    };
    const scrolling = await panelScroll(targets.rightColumn);
    emit(viewport, "layout at the top of the page", { columns, panelScroll: scrolling, places: found, ...(await measure(page, null)) });
    Object.assign(row, {
      sideBySide: columns.sideBySide,
      leftWidth: columns.leftWidth,
      rightWidth: columns.rightWidth,
      card2Width: found.card2.width,
      card2ChartWidth: found.card2Chart.width,
      panelChartWidth: found.panelChart.width,
      panelDocTop: found.rightColumn.docTop,
      pageHeight: (await measure(page, null)).page.scrollHeight,
      panelSticky: scrolling.position === "sticky",
      panelHeightTop: found.rightColumn.shown,
      panelScrolls: scrolling.scrolls,
      panelClientHeight: scrolling.clientHeight,
      panelScrollHeight: scrolling.scrollHeight,
      card2HeaderWraps: found.card2Header.height > 30,
    });
    await shoot(page, viewport, "arrival");
  });

  await step("second answer", async () => {
    await scrollUnderHeader(card2);
    const at = await places(page, {
      answer2: await handle(answers.nth(1)),
      card2: await handle(card2),
      card2Header: await handle(card2.locator("h3")),
      card2Chart: await plotOf(card2, V2_CHART),
      panel: await handle(panel),
      panelHeader: await handle(panelHeader),
      panelStatus: await handle(panelStatus),
      panelKpi: await handle(component(panel, V2_KPI)),
      panelChart: await plotOf(panel, V2_CHART),
    });
    emit(viewport, "second answer: its card's top scrolled under the header", { scrollY: await scrollY(), panelScroll: await panelScroll(await handle(panel)), places: at });
    Object.assign(row, {
      latestPanelShown: at.panel.shown,
      latestPanelStatusVisible: at.panelStatus.whole,
      latestPanelHeaderWhole: at.panelHeader.whole,
      latestPanelChartShown: at.panelChart.shown,
      latestCard2ChartShown: at.card2Chart.shown,
    });
    await shoot(page, viewport, "second-answer");
  });

  // A mouse wheel over the panel while the latest answer is in view: what scrolls, tick by tick.
  await step("wheel over the panel", async () => {
    const box = await (await handle(panel)).boundingBox();
    const vh = viewport.height;
    // The panel's middle where it shows; where it does not, the same x at the window's middle.
    const top = Math.max(box.y, 60);
    const bottom = Math.min(box.y + box.height, vh);
    const y = bottom > top ? Math.round((top + bottom) / 2) : Math.round(vh / 2);
    const x = Math.round(box.x + box.width / 2);
    await page.mouse.move(x, y);
    const under = await page.evaluate(([x, y]) => {
      const element = document.elementFromPoint(x, y);
      return { inPanel: Boolean(element?.closest(".bg-muted")), tag: element?.tagName.toLowerCase() ?? null };
    }, [x, y]);
    const panelHandle = await handle(panel);
    // Every wheel event the page receives, where it lands and whether something cancels it.
    // A wheel event seen on the way down (capture) and on the way back up (bubble): one that does
    // not come back up was stopped on the way, and `cancelled` is whether a handler called
    // preventDefault (a chart zooming instead of the page scrolling).
    await panelHandle.evaluate((panel) => {
      window.__wheels = [];
      window.addEventListener("wheel", (event) => window.__wheels.push({ deltaY: event.deltaY, inPanel: panel.contains(event.target), back: false, cancelled: null }), { capture: true, passive: true });
      window.addEventListener("wheel", (event) => {
        const last = window.__wheels.at(-1);
        if (last) Object.assign(last, { back: true, cancelled: event.defaultPrevented });
      }, { passive: true });
    });
    // What the pointer is over: inside a chart's plot (its canvas) or not, and in the panel or not.
    const underPointer = () =>
      panelHandle.evaluate(
        (panel, [x, y]) => {
          const element = document.elementFromPoint(x, y);
          return { tag: element?.tagName.toLowerCase() ?? null, chart: element?.tagName === "CANVAS", inPanel: Boolean(element && panel.contains(element)) };
        },
        [x, y],
      );
    const positions = async () => ({ scrollY: await scrollY(), panelScrollTop: (await panelScroll(panelHandle)).scrollTop });
    // `pauseMs` 0: the ticks follow each other as fast as the browser takes them (one gesture, the
    // way a quick flick of a wheel arrives); otherwise each tick comes after a pause (separate
    // gestures). After a tick the record waits up to a second for anything to move, then settles.
    const sequence = async (pauseMs) => {
      await page.evaluate(() => {
        for (const element of document.querySelectorAll("#root *")) if (element.scrollTop > 0) element.scrollTop = 0;
        window.__wheels.length = 0;
      });
      await scrollUnderHeader(card2);
      const start = await positions();
      const ticks = [];
      for (const delta of [200, 200, 200, 200, -200, -200, -200, -200]) {
        const before = await positions();
        const over = await underPointer();
        await page.mouse.wheel(0, delta);
        await page
          .waitForFunction(
            ([panel, y, top]) => Math.round(window.scrollY) !== y || Math.round(panel.scrollTop) !== top,
            [panelHandle, before.scrollY, before.panelScrollTop],
            { timeout: 1000 },
          )
          .catch(() => {});
        const settled = await settle(page);
        ticks.push({ delta, over, settled, ...(await positions()) });
        if (pauseMs > 0) await page.waitForTimeout(pauseMs);
      }
      const events = await page.evaluate(() => window.__wheels.splice(0));
      ticks.forEach((tick, index) => Object.assign(tick, { event: events[index] ?? null }));
      return { start, ticks, events: { count: events.length, inPanel: events.filter((event) => event.inPanel).length, stopped: events.filter((event) => !event.back).length, cancelled: events.filter((event) => event.cancelled).length } };
    };
    const quick = await sequence(0);
    const paused = await sequence(1200);
    const scroll = await panelScroll(panelHandle);
    emit(viewport, "wheel over the panel (latest answer in view)", { pointer: { x, y, ...under }, panelMaxScroll: scroll.scrollHeight - scroll.clientHeight, quick, paused });
    // "+ 1914/0" is a tick down after which the page is at 1914 and the panel at 0; "c" marks a tick
    // that started with a chart's plot under the pointer, "x" one a handler cancelled.
    const brief = (run) =>
      [
        `start ${run.start.scrollY}/${run.start.panelScrollTop}`,
        ...run.ticks.map((tick) => `${tick.delta > 0 ? "+" : "-"}${tick.over.chart ? "c" : ""}${tick.event?.cancelled || tick.event?.back === false ? "x" : ""} ${tick.scrollY}/${tick.panelScrollTop}`),
      ].join(", ");
    row.wheelQuick = brief(quick);
    row.wheelPaused = brief(paused);
    row.wheelPointerInPanel = under.inPanel;
  });

  // Tab from the composer into the panel, with the latest answer in view.
  await step("tab into the panel", async () => {
    await resetScroll();
    await scrollUnderHeader(card2);
    const composer = main.locator("textarea").first();
    await composer.focus();
    const startY = await scrollY();
    const stops = [];
    let entered = false;
    for (let index = 0; index < 14; index += 1) {
      await page.keyboard.press("Tab");
      await settle(page);
      const stop = await page.evaluate((title) => {
        const element = document.activeElement;
        const panel = [...document.querySelectorAll("#root main h2")].find((heading) => heading.textContent.trim() === title)?.closest(".bg-muted") ?? null;
        const rect = element.getBoundingClientRect();
        const header = document.querySelector("#root header.sticky");
        const headerBottom = header ? header.getBoundingClientRect().bottom : 0;
        let top = Math.max(rect.top, headerBottom);
        let bottom = Math.min(rect.bottom, window.innerHeight);
        for (let up = element.parentElement; up && up.id !== "root"; up = up.parentElement) {
          if (getComputedStyle(up).overflowY === "visible") continue;
          const upRect = up.getBoundingClientRect();
          top = Math.max(top, upRect.top);
          bottom = Math.min(bottom, upRect.bottom);
        }
        return {
          tag: element.tagName.toLowerCase(),
          role: element.getAttribute("role"),
          label: element.getAttribute("aria-label") ?? element.textContent.replace(/\s+/g, " ").trim().slice(0, 40),
          isPanel: element === panel,
          inPanel: Boolean(panel?.contains(element)),
          whollyShown: bottom - top >= rect.height - 1,
          focusVisible: element.matches(":focus-visible"),
          // The focus indicator: an outline or a box-shadow ring.
          outline: `${getComputedStyle(element).outlineStyle} ${getComputedStyle(element).outlineWidth}`,
          ring: getComputedStyle(element).boxShadow === "none" ? null : getComputedStyle(element).boxShadow.slice(0, 60),
          // Tailwind's variable the outline utilities read the outline style from.
          twOutlineStyle: getComputedStyle(element).getPropertyValue("--tw-outline-style").trim() || null,
          scrollY: Math.round(window.scrollY),
          panelScrollTop: panel ? Math.round(panel.scrollTop) : null,
        };
      }, DRAFT_TITLE);
      stops.push(stop);
      if (stop.isPanel) await shoot(page, viewport, "tab-region");
      if (stop.inPanel && stop.label === "儲存到儀表板列表") await shoot(page, viewport, "tab-save");
      if (stop.inPanel || stop.isPanel) entered = true;
      if (entered && !stop.inPanel && !stop.isPanel) break;
      if (entered && stops.filter((item) => item.inPanel || item.isPanel).length >= 8) break;
    }
    emit(viewport, "tab from the composer into the panel (latest answer in view)", { startScrollY: startY, stops });
    const first = stops.find((stop) => stop.inPanel || stop.isPanel);
    row.tabFirstInPanel = first ? `${first.isPanel ? "panel region" : first.label}` : null;
    row.tabPageJump = first ? first.scrollY - startY : null;
    row.tabAllShown = stops.filter((stop) => stop.inPanel || stop.isPanel).every((stop) => stop.whollyShown);
    row.tabOutline = [...new Set(stops.filter((stop) => stop.focusVisible).map((stop) => `${stop.outline}/${stop.twOutlineStyle}`))].join(" ");
    await page.evaluate(() => document.activeElement?.blur());
  });

  // The panel's content against its box at the top of the page and at its own end.
  await step("panel overflow", async () => {
    await resetScroll();
    const panelHandle = await handle(panel);
    const atTop = await panelScroll(panelHandle);
    let atEnd = null;
    if (atTop.scrolls) {
      await panelHandle.evaluate((element) => {
        element.scrollTop = element.scrollHeight;
      });
      await settle(page);
      atEnd = { scroll: await panelScroll(panelHandle), places: await places(page, { panelChart: await plotOf(panel, V2_CHART) }) };
      await shoot(page, viewport, "panel-scrolled-to-end");
    }
    emit(viewport, "panel overflow", { atTop, atEnd });
    row.panelEndChartWhole = atEnd ? atEnd.places.panelChart.whole : null;
  });

  await step("sources", async () => {
    await resetScroll();
    await scrollUnderHeader(card2);
    row.sourceAnswerKpi = await openSource(card2, "answer (left column)", "kpi", "source-answer-kpi");
    row.sourcePanelKpi = await openSource(panel, "panel", "kpi", "source-panel-kpi");
    await resetScroll();
    await scrollUnderHeader(card2);
    row.sourcePanelChart = await openSource(panel, "panel", "chart", "source-panel-chart");
  });

  // Saving from the LAST answer's card (the runbook: only that one), then both places.
  await step("save", async () => {
    await resetScroll();
    const before = {
      card2: await stateOf(card2, card2.locator("h3").first()),
      panel: await stateOf(panel, panelTitle),
    };
    await scrollUnderHeader(card2);
    const save = card2.getByRole("button", { name: "儲存到儀表板列表" });
    await waitVisible(save, "the last answer's 儲存到儀表板列表 button");
    await save.click();
    await waitVisible(card2.getByText(SAVED), "the saved notice in the last answer's card");
    const panelSaved = await panel.getByText(SAVED).first().waitFor({ state: "visible", timeout: 15_000 }).then(() => true, () => false);
    await settle(page);
    const at = await places(page, {
      card2Status: await handle(card2Status),
      card2Notice: await handle(card2.getByText(SAVED)),
      card2Link: await handle(card2.getByRole("link", { name: "開啟儀表板" })),
      panelStatus: await handle(panelStatus),
      panelNotice: panelSaved ? await handle(panel.getByText(SAVED)) : null,
      panelLink: panelSaved ? await handle(panel.getByRole("link", { name: "開啟儀表板" })) : null,
    });
    const both = Boolean(at.card2Status.whole && at.card2Notice.whole && at.panelStatus.whole && at.panelNotice?.whole);
    emit(viewport, "saved: both places while the last answer's card is in view", {
      scrollY: await scrollY(),
      panelScroll: await panelScroll(await handle(panel)),
      before,
      after: { card2: await stateOf(card2, card2.locator("h3").first()), panel: await stateOf(panel, panelTitle) },
      panelShowsSaved: panelSaved,
      bothStatusesShown: both,
      places: at,
    });
    Object.assign(row, {
      savedCard2Status: at.card2Status.whole,
      savedCard2Notice: at.card2Notice.whole,
      savedPanelStatus: at.panelStatus.whole,
      savedPanelNotice: Boolean(at.panelNotice?.whole),
      savedBoth: both,
      savedPanelStatusText: (await stateOf(panel, panelTitle)).header.slice(0, 60),
    });
    await shoot(page, viewport, "saved");
  });

  emit(viewport, "summary row", { row });
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
process.exitCode = failures.length > 0 ? 1 : 0;
