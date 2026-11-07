// The projector walkthrough of the demo script's screens (Laysi/trustiot-v2#1179), run in the
// Playwright image (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`). It
// walks sign-in, home, device page, conversation (question, waiting, answer, the answer's dashboard
// card, its source panel, saving) and the dashboard page in Traditional Chinese (zh-TW,
// Asia/Taipei) at the CSS viewports a presenter gets on a projector: 1920×1080 and 1280×720
// screens at 100 %, 125 % and 150 % browser zoom, full screen and windowed (120 device px of tabs
// and address bar, which do not zoom). Every step prints one JSON line of DOM measurements: page
// and #root horizontal overflow, elements that leave the window, clipped text, the sticky bars and
// the height left between them, and the step's key element (does it fit, how much is cut, how the
// cut part is reached). Screenshots (JPEG) are only taken for the sizes in SHOT_VIEWPORTS and are
// printed base64 after the JSON lines, between markers, for the reviewer's eyes; they are never
// part of the committed record. What only the preview draws (its toolbar, the draft-list alert,
// the composer's completion hint and template button, the home page's setup checklist and
// suggestions) is hidden, so the numbers are the product's layout; see previewEmulation.
//
// Usage: docs/test-reports/data/2026-11-07-1179-projector-walkthrough/run.sh (from the repository
// root). VIEWPORTS=<id,id> limits the run to some viewports; SHOTS=0 takes no screenshots;
// PREVIEW_AS_IS=1 keeps the preview-only parts.
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
const PREVIEW_CONVERSATION = "/conversations/3e7b1c94-8a52-4d60-b9f3-0c6d2a5e7f18";
/** The preview's fictional user (src/test/fake-identity.ts) and its first sign-in code. */
const PREVIEW_EMAIL = "lin.mei@example.com";
const FIRST_CODE = "10000000";
/** The runbook's main request sentence (docs/runbooks/demo-presentation.md, 步驟 3). */
const QUESTION = "做一個儀表板：最近 7 天每天的平均溫度與最高溫度，用長條圖比較，另外用一個數字顯示現在的溫度和濕度。以台北時間分天。";
/** Its change sentence (步驟 4); the walk adds the preview's `!slow` so the question keeps running. */
const FOLLOW_UP = "改成每小時，只看濕度。";

/**
 * Screen × zoom × full screen or windowed. The CSS size is the screen's device pixels (minus 120
 * of browser chrome when windowed) divided by the zoom; deviceScaleFactor is the zoom.
 */
const SCREENS = [
  { screen: "1920x1080", w: 1920, h: 1080 },
  { screen: "1280x720", w: 1280, h: 720 },
];
const CHROME_PX = 120;
const ALL = [];
for (const { screen, w, h } of SCREENS) {
  for (const mode of ["full", "windowed"]) {
    for (const zoom of [100, 125, 150]) {
      const dsf = zoom / 100;
      const height = mode === "full" ? h : h - CHROME_PX;
      ALL.push({
        id: `${mode === "full" ? "fs" : "win"}${screen.split("x")[0]}-${zoom}`,
        screen,
        mode,
        zoom,
        width: Math.floor(w / dsf),
        height: Math.floor(height / dsf),
        dsf,
        scheme: "light",
      });
    }
  }
}
// Dark at a common size and at the smallest.
for (const id of ["fs1280-100", "win1280-150"]) {
  const base = ALL.find((viewport) => viewport.id === id);
  ALL.push({ ...base, id: `${id}-dark`, scheme: "dark" });
}
const only = (process.env.VIEWPORTS ?? "").split(",").filter(Boolean);
const VIEWPORTS = only.length > 0 ? ALL.filter((viewport) => only.includes(viewport.id)) : ALL;
const SHOT_VIEWPORTS = new Set(process.env.SHOTS === "0" ? [] : ["win1280-150", "win1280-125", "win1280-100", "fs1280-100"]);
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
  if (!SHOT_VIEWPORTS.has(viewport.id) || viewport.scheme !== "light") return;
  const image = await page.screenshot({ type: "jpeg", quality: 55, scale: "css" });
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

/** Hovers the chart inside `container` until its tooltip shows; where the tooltip is. */
async function tooltip(page, container) {
  const chart = container.locator("canvas").first();
  await waitVisible(chart, "a chart canvas");
  await chart.scrollIntoViewIfNeeded();
  const deadline = Date.now() + WAIT_MS;
  const tip = container.getByTestId("chart-tooltip");
  let step = 0;
  while (Date.now() < deadline) {
    const box = await chart.boundingBox();
    if (box) {
      await page.mouse.move(box.x + box.width * (0.55 + (step % 5) * 0.03), box.y + box.height * 0.5);
    }
    step += 1;
    try {
      await tip.first().waitFor({ state: "visible", timeout: 1000 });
      break;
    } catch {
      // Move a little and look again.
    }
  }
  if (!(await tip.first().isVisible())) return { shown: false };
  return tip.first().evaluate((element) => {
    const rect = element.getBoundingClientRect();
    const vw = document.documentElement.clientWidth;
    return {
      shown: true,
      text: element.textContent.replace(/\s+/g, " ").trim().slice(0, 120),
      left: Math.round(rect.left),
      right: Math.round(rect.right),
      top: Math.round(rect.top),
      bottom: Math.round(rect.bottom),
      insideWindow: rect.left >= 0 && rect.right <= vw && rect.top >= 0 && rect.bottom <= window.innerHeight,
    };
  });
}

/**
 * Runs in every page before the app: hides what the UI preview draws and the product does not, so
 * the measurements are the product's layout (set PREVIEW_AS_IS=1 to measure the preview as it is).
 * - the preview's toolbar above #root;
 * - the conversation page's 「讀不到這段對話的草稿」 alert: the preview has no draft-list endpoint;
 * - the composer's grey-completion hint and 「問句範本」 button: completion and templates exist only
 *   in the preview (their providers are pending APIs);
 * - on the home page, the setup checklist and the suggested questions (pending APIs), once the
 *   walk has recorded the preview's home as it is (sessionStorage `walkthrough-hide-home-slots`).
 * `window.__walkthroughApply()` applies it at once; `window.__walkthroughHidden` counts what it hid.
 */
function previewEmulation(emulate) {
  const hidden = new Set();
  const mark = (element, reason, on) => {
    if (on && !element.hasAttribute("data-walkthrough-hidden")) element.setAttribute("data-walkthrough-hidden", reason);
    if (!on && element.getAttribute("data-walkthrough-hidden") === reason) element.removeAttribute("data-walkthrough-hidden");
    if (on) hidden.add(reason);
  };
  const apply = () => {
    if (!emulate) return;
    for (const alert of document.querySelectorAll("[data-slot=alert]")) {
      mark(alert, "draft-list-alert", alert.textContent.startsWith("讀不到這段對話的草稿"));
    }
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
    let hideSlots = false;
    try {
      hideSlots = sessionStorage.getItem("walkthrough-hide-home-slots") === "1";
    } catch {
      // No storage: the slots stay.
    }
    const heading = [...document.querySelectorAll("#root main h1")].find((h) => h.textContent.trim() === "首頁");
    const grid = heading?.closest("section")?.parentElement;
    if (grid) {
      for (const child of grid.children) {
        const slot = child.textContent.includes("開始使用 TrustIoT") || child.textContent.includes("試試這些問題");
        mark(child, "home-slot", hideSlots && slot);
      }
    }
  };
  window.__walkthroughApply = apply;
  window.__walkthroughHidden = () => [...hidden];
  document.addEventListener("DOMContentLoaded", () => {
    const style = document.createElement("style");
    style.textContent = [
      "#ui-preview-toolbar{display:none!important}",
      ...(emulate ? ['[data-walkthrough-hidden]{display:none!important}', 'button[aria-label="問句範本（/）"]{display:none!important}'] : []),
    ].join("\n");
    document.head.append(style);
    new MutationObserver(apply).observe(document.body, { subtree: true, childList: true, characterData: true });
    apply();
  });
}

/** The preview-only home slots, as the preview draws them (heights and first words). */
async function previewHomeSlots(page) {
  return page.evaluate(() => {
    const heading = [...document.querySelectorAll("#root main h1")].find((h) => h.textContent.trim() === "首頁");
    const grid = heading?.closest("section")?.parentElement;
    if (!grid) return { found: false };
    const slots = [...grid.children].filter((child) => child.textContent.includes("開始使用 TrustIoT") || child.textContent.includes("試試這些問題"));
    return {
      found: true,
      slots: slots.map((slot) => ({ height: Math.round(slot.getBoundingClientRect().height), text: slot.textContent.replace(/\s+/g, " ").trim().slice(0, 60) })),
    };
  });
}

async function hidePreviewSlots(page) {
  await page.evaluate(() => {
    try {
      sessionStorage.setItem("walkthrough-hide-home-slots", "1");
    } catch {
      // No storage: the slots stay, and the record says what was hidden.
    }
    window.__walkthroughApply();
  });
}


async function homeFacts(page) {
  return page.evaluate(() => {
    const vh = window.innerHeight;
    const rectOf = (element) => {
      if (!element) return null;
      const rect = element.getBoundingClientRect();
      return { top: Math.round(rect.top), bottom: Math.round(rect.bottom), inWindow: rect.top >= 0 && rect.bottom <= vh };
    };
    const composer = document.querySelector("#root main textarea")?.closest("form") ?? document.querySelector("#root main textarea");
    const recentHeading = [...document.querySelectorAll("#root main h2")].find((h) => h.textContent.includes("最近的對話"));
    const recentList = recentHeading?.closest("section")?.querySelector("ul, ol");
    const firstItem = recentList?.querySelector("li");
    return {
      composer: rectOf(composer),
      recentHeading: rectOf(recentHeading),
      firstRecent: rectOf(firstItem),
      recentCount: recentList?.querySelectorAll(":scope > li").length ?? 0,
    };
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
  // What the preview draws and the product does not is hidden (previewEmulation).
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
  /**
   * Scrolls `component`'s top just under the header, clicks its 資料來源 button and records where
   * the panel opens (the page scrolls only as far as focusing the panel's heading needs), then
   * where it is with its own top scrolled under the header. `button` names a button in the panel
   * whose place is recorded too.
   */
  const openSource = async (component, title, where, shotName, button) => {
    await component.evaluate((element) => {
      const header = document.querySelector("#root header.sticky");
      window.scrollBy(0, element.getBoundingClientRect().top - (header?.getBoundingClientRect().height ?? 0) - 8);
    });
    const scrollBefore = await page.evaluate(() => Math.round(window.scrollY));
    await component.getByRole("button", { name: `「${title}」的資料來源` }).click();
    const panel = component.getByRole("region", { name: "資料來源" });
    await waitVisible(panel, `the source panel of ${title}`);
    await waitVisible(panel.getByText("筆數", { exact: true }), "the panel's 筆數 line");
    if (button) await waitVisible(panel.getByRole("button", { name: button }), `${button} in the panel`);
    const handle = await panel.elementHandle();
    const place = async () => (button ? { buttonInPanel: await inWindow(panel.getByRole("button", { name: button })) } : {});
    emit(viewport, `${where}: source panel as opened`, { scrollBefore, ...(await place()), ...(await measure(page, handle, { noScroll: true })) });
    await shoot(page, viewport, shotName);
    const scrolled = await measure(page, handle);
    emit(viewport, `${where}: source panel scrolled to its top`, { ...(await place()), ...scrolled });
    await shoot(page, viewport, `${shotName}-top`);
    return panel;
  };

  await page.goto(`${BASE}/`);
  await waitVisible(page.getByRole("heading", { level: 1, name: "首頁" }), "the home page's heading 首頁");
  emit(viewport, "language", {
    lang: await page.evaluate(() => document.documentElement.lang),
    languages: await page.evaluate(() => navigator.languages),
    timeZone: await page.evaluate(() => Intl.DateTimeFormat().resolvedOptions().timeZone),
    toolbarHidden: await page.evaluate(() => getComputedStyle(document.getElementById("ui-preview-toolbar")).display === "none"),
  });

  // 1. Sign-in: sign out from the account menu, then the email step and the code step.
  await step("login", async () => {
    await page.getByRole("button", { name: "帳號" }).click();
    await page.getByRole("menuitem", { name: "登出" }).click();
    const title = page.getByRole("heading", { name: "登入 TrustIoT" });
    await waitVisible(title, "the sign-in page's heading 登入 TrustIoT");
    const emailBox = page.getByRole("textbox", { name: "Email" });
    await waitVisible(emailBox, "the email field");
    const card = await title.evaluateHandle((element) => element.closest("[data-slot=card]") ?? element.parentElement.parentElement);
    emit(viewport, "login: email step (sign-in card)", {
      sendButton: await inWindow(page.getByRole("button", { name: "寄送驗證碼" })),
      ...(await measure(page, card, { noScroll: true })),
    });
    await shoot(page, viewport, "login-email");
    await emailBox.fill(PREVIEW_EMAIL);
    await page.getByRole("button", { name: "寄送驗證碼" }).click();
    const codeBox = page.getByRole("textbox", { name: "驗證碼" });
    await waitVisible(codeBox, "the code field after 寄送驗證碼");
    const codeCard = await codeBox.evaluateHandle((element) => element.closest("[data-slot=card]") ?? element.closest("form").parentElement);
    emit(viewport, "login: code step (sign-in card)", {
      signInButton: await inWindow(page.getByRole("button", { name: "登入", exact: true })),
      ...(await measure(page, codeCard, { noScroll: true })),
    });
    await shoot(page, viewport, "login-code");
    await codeBox.fill(FIRST_CODE);
    await page.getByRole("button", { name: "登入", exact: true }).click();
    await waitVisible(page.getByRole("heading", { level: 1, name: "首頁" }), "the home page after signing in");
  });

  // 2. Home: the composer and the recent conversations, as the preview draws it and without the
  // preview-only slots (production has neither the setup checklist nor the suggestions yet).
  await step("home", async () => {
    if (new URL(page.url()).pathname !== "/") await page.goto(`${BASE}/`);
    await waitVisible(page.getByRole("heading", { level: 1, name: "首頁" }), "the home page's heading");
    await waitVisible(main.getByRole("heading", { name: "最近的對話" }), "the recent conversations heading");
    await waitVisible(main.locator("section:has(h2:has-text('最近的對話')) li").first(), "a recent conversation");
    const slots = await previewHomeSlots(page);
    const composer = await main.locator("textarea").first().evaluateHandle((element) => element.closest("form") ?? element);
    emit(viewport, "home (preview, with its setup and suggestion slots)", { slots, facts: await homeFacts(page), ...(await measure(page, composer, { noScroll: true })) });
    if (EMULATE) {
      await hidePreviewSlots(page);
      emit(viewport, "home (production layout: preview slots hidden)", { facts: await homeFacts(page), ...(await measure(page, composer, { noScroll: true })) });
    }
    await shoot(page, viewport, "home");
  });

  // 3. Device page: the latest readings, the status label, the history chart at 1 hour and 7 days,
  // the table, a tooltip; a gap (chiller-2) and a stale device (boiler-1).
  const history = main.locator("section[aria-labelledby=device-history-title]");
  const pressed = (label) =>
    page.waitForFunction(
      (label) =>
        document.querySelector("section[aria-labelledby=device-history-title] fieldset [aria-pressed=true]")?.textContent?.includes(label),
      label,
      { timeout: WAIT_MS },
    ).catch(() => {
      throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for the range ${label} to be pressed`);
    });
  const openDevice = async (name) => {
    await page.goto(`${BASE}/devices`);
    await waitVisible(main.getByRole("link", { name }).first(), `${name} in the device list`);
    // A presenter scrolls the list to find the device; the page keeps that scroll position.
    await main.getByRole("link", { name }).first().scrollIntoViewIfNeeded();
    const listScrollY = await page.evaluate(() => Math.round(window.scrollY));
    await main.getByRole("link", { name }).first().click();
    await waitVisible(main.getByRole("heading", { name: "目前讀數" }), `${name}'s 目前讀數 card`);
    await waitVisible(history.locator("canvas").first(), `${name}'s history chart`);
    return listScrollY;
  };
  /** The history card's range buttons, chart plot and the 以表格顯示 row: can they be seen together? */
  const historySpans = async () => {
    const range = await history.locator("fieldset").first().elementHandle();
    const plot = await history.locator("canvas").first().evaluateHandle((element) => element.closest(".h-72") ?? element.parentElement);
    const title = await history.locator("#device-history-title").elementHandle();
    const tableButton = await history.getByRole("button", { name: /^以表格顯示/ }).elementHandle();
    return {
      titleToPlot: await measureSpan(page, title, plot),
      rangeToPlot: await measureSpan(page, range, plot),
      rangeToTableButton: await measureSpan(page, range, tableButton),
      plot: await measureSpan(page, plot, plot),
    };
  };
  await step("device", async () => {
    const listScrollY = await openDevice("cold-room-1");
    const latestCard = await main.locator("section[aria-labelledby=device-latest]").elementHandle();
    emit(viewport, "device: arrival (latest card)", { listScrollY, ...(await measure(page, latestCard, { noScroll: true })) });
    await page.evaluate(() => window.scrollTo(0, 0));
    emit(viewport, "device: top of page (status label and latest card)", await measure(page, latestCard, { noScroll: true }));
    await shoot(page, viewport, "device-top");

    await history.getByRole("button", { name: "1 小時", exact: true }).click();
    await pressed("1 小時");
    await waitVisible(history.locator("canvas").first(), "the 1-hour chart's canvas");
    emit(viewport, "device: history card, 最近 1 小時", { spans: await historySpans(), ...(await measure(page, await history.elementHandle())) });
    // The range buttons just under the header: what a presenter shows for step 2.
    await page.evaluate(() => {
      const range = document.querySelector("section[aria-labelledby=device-history-title] fieldset");
      window.scrollBy(0, range.getBoundingClientRect().top - document.querySelector("#root header").getBoundingClientRect().height - 8);
    });
    await shoot(page, viewport, "device-chart-1h");
    emit(viewport, "device: tooltip, 最近 1 小時", { tooltip: await tooltip(page, history) });
    await page.mouse.move(0, 0);

    await history.getByRole("button", { name: "7 天", exact: true }).click();
    await pressed("7 天");
    await waitVisible(history.locator("canvas").first(), "the 7-day chart's canvas");
    emit(viewport, "device: history card, 最近 7 天", { spans: await historySpans(), ...(await measure(page, await history.elementHandle())) });

    const showTable = history.getByRole("button", { name: /^以表格顯示/ });
    await waitVisible(showTable, "the 以表格顯示 button");
    await showTable.click();
    const table = history.locator("table").first();
    await waitVisible(table, "the history table");
    const tableWrapper = await table.evaluateHandle((element) => element.parentElement);
    emit(viewport, "device: 以表格顯示 (7 天)", await measure(page, tableWrapper));
    await history.getByRole("button", { name: "隱藏表格" }).click();

    // The gap: chiller-2's history has a hole.
    await openDevice("chiller-2");
    await history.getByRole("button", { name: "7 天", exact: true }).click();
    await pressed("7 天");
    await waitVisible(history.locator("canvas").first(), "chiller-2's 7-day chart");
    const gapTexts = await history.evaluate((element) => element.textContent.match(/[0-9,.]+ ?(分鐘|小時|天)[^。]*?沒有讀數/g) ?? []);
    emit(viewport, "device: gap (chiller-2, 最近 7 天)", { gapTexts, spans: await historySpans(), ...(await measure(page, await history.elementHandle())) });
    await page.evaluate(() => {
      const range = document.querySelector("section[aria-labelledby=device-history-title] fieldset");
      window.scrollBy(0, range.getBoundingClientRect().top - document.querySelector("#root header").getBoundingClientRect().height - 8);
    });
    await shoot(page, viewport, "device-gap-7d");
    emit(viewport, "device: tooltip, 最近 7 天 (chiller-2)", { tooltip: await tooltip(page, history) });
    await page.mouse.move(0, 0);

    // The stale device's status label.
    await openDevice("boiler-1");
    await page.evaluate(() => window.scrollTo(0, 0));
    const label = main.getByText(/^資料停滯/).first();
    await waitVisible(label, "boiler-1's 資料停滯 label");
    emit(viewport, "device: status label (boiler-1, 資料停滯)", {
      label: await label.evaluate((element) => element.textContent.replace(/\s+/g, " ").trim()),
      ...(await measure(page, await label.elementHandle(), { noScroll: true })),
    });
  });

  // 4. Conversation: ask from home, the waiting state, the answer; then ask again in the
  // conversation (step 4 of the script) with a question that keeps running, and stop it.
  await step("ask", async () => {
    await page.goto(`${BASE}/`);
    const box = main.getByRole("textbox", { name: "問 TrustIoT" });
    await waitVisible(box, "the home composer 問 TrustIoT");
    await box.fill(QUESTION);
    const homeWaiting = main.getByText("正在查詢讀數…").first().waitFor({ state: "visible", timeout: WAIT_MS }).then(() => true, () => false);
    await main.getByRole("button", { name: "送出" }).click();
    if (await homeWaiting) {
      const status = await main.getByText("正在查詢讀數…").first().elementHandle();
      emit(viewport, "home: sending (正在查詢讀數… in the home composer)", await measure(page, status, { noScroll: true }));
      await shoot(page, viewport, "home-sending");
    }
    try {
      await page.waitForURL(/\/conversations\/[0-9a-f-]{36}$/, { timeout: WAIT_MS });
    } catch {
      throw new Error("timed out after 60 s waiting for the new conversation's page");
    }
    // The answer comes 2.5 s after the question in the preview; look for the waiting line on the
    // conversation page while it lasts.
    const pending = main.getByRole("article", { name: "TrustIoT 的回答" }).or(main.getByText(/^正在(查詢讀數|寫回答)…$/)).first();
    await waitVisible(pending, "the waiting line or the answer on the new conversation's page");
    const waitingText = await main.getByText(/^正在(查詢讀數|寫回答)…$/).first().textContent({ timeout: 1000 }).catch(() => null);
    if (waitingText) {
      const line = await main.getByText(/^正在(查詢讀數|寫回答)…$/).first().elementHandle();
      emit(viewport, `conversation: waiting (${waitingText})`, await measure(page, line, { noScroll: true }));
      await shoot(page, viewport, "conversation-waiting");
    } else {
      emit(viewport, "conversation: waiting line already gone", {});
    }
    const answer = main.getByRole("article", { name: "TrustIoT 的回答" }).last();
    await waitVisible(answer, "the answer to the new question");
    emit(viewport, "conversation: answer (preview text, no dashboard)", await measure(page, await answer.elementHandle(), { noScroll: true }));
  });

  // The answer with its dashboard card: the preview's conversation whose answer wrote a draft.
  await step("answer card", async () => {
    await page.goto(`${BASE}${PREVIEW_CONVERSATION}`);
    const article = main.getByRole("article", { name: "TrustIoT 的回答" }).first();
    await waitVisible(article, "the preview conversation's answer");
    const card = main.locator("section[data-answer-dashboard]").first();
    await waitVisible(card, "the answer's dashboard card");
    await waitVisible(card.locator("canvas").first(), "a chart in the answer's dashboard card");
    const cardHandle = await card.elementHandle();
    // The preview has no draft-list endpoint, so the page says it cannot read the drafts; the
    // product does not show this.
    const previewOnly = await main.getByText(/^讀不到這段對話的草稿/).first().evaluate((element) => {
      const box = element.closest("[role=alert], [data-slot=alert]") ?? element;
      return { text: box.textContent.replace(/\s+/g, " ").trim(), height: Math.round(box.getBoundingClientRect().height) };
    }).catch(() => null);
    emit(viewport, "conversation: arrival", { previewOnly, ...(await measure(page, cardHandle, { noScroll: true })) });
    await shoot(page, viewport, "conversation-arrival");
    const component = (title) => card.getByRole("heading", { level: 3, name: title }).locator("xpath=ancestor::section[1]");
    const first = component("Power by line");
    await waitVisible(first.locator("canvas").first(), "Power by line's chart in the card");
    const plot = await first.locator("canvas").first().evaluateHandle((element) => element.closest(".h-72") ?? element.parentElement);
    const cardTitle = await card.locator("h3").first().elementHandle();
    const answerText = await article.elementHandle();
    emit(viewport, "conversation: dashboard card", {
      spans: {
        answerToFirstPlot: await measureSpan(page, answerText, plot),
        cardTitleToFirstPlot: await measureSpan(page, cardTitle, plot),
        firstComponent: await measureSpan(page, await first.elementHandle(), await first.elementHandle()),
        plot: await measureSpan(page, plot, plot),
      },
      ...(await measure(page, cardHandle)),
    });
    await shoot(page, viewport, "conversation-card");
    emit(viewport, "conversation: first chart component", await measure(page, await first.elementHandle()));
    emit(viewport, "conversation: card tooltip", { tooltip: await tooltip(page, first) });
    await page.mouse.move(0, 0);

    // The source panel of "Daily average power", opened with the component's top just under the
    // header, as a presenter who shows the chart and then clicks 資料來源 has it.
    const daily = component("Daily average power");
    await waitVisible(daily, "the Daily average power component in the card");
    const panel = await openSource(daily, "Daily average power", "conversation", "conversation-source-panel");
    await panel.getByRole("button", { name: "關閉來源" }).click();

    // Save to the dashboard list, and the saved state.
    const save = card.getByRole("button", { name: "儲存到儀表板列表" });
    await waitVisible(save, "the 儲存到儀表板列表 button");
    emit(viewport, "conversation: save button (card title scrolled under the header)", await measure(page, await save.elementHandle()));
    await save.click();
    const saved = card.getByText(/已儲存為儀表板列表裡的第 \d+ 版/);
    await waitVisible(saved, "the saved message 已儲存為儀表板列表裡的第 1 版");
    const outcome = await saved.evaluateHandle((element) => element.closest("[role=status], [role=alert]") ?? element.parentElement.parentElement);
    emit(viewport, "conversation: saved state", {
      badge: await card.locator("h3").first().evaluate((h) => h.parentElement.textContent.replace(/\s+/g, " ").trim()),
      openLink: await inWindow(card.getByRole("link", { name: "開啟儀表板" })),
      ...(await measure(page, outcome, { noScroll: true })),
    });
    await shoot(page, viewport, "conversation-saved");

    // 5. The dashboard page, opened from the saved state.
    const conversationScrollY = await page.evaluate(() => Math.round(window.scrollY));
    await card.getByRole("link", { name: "開啟儀表板" }).click();
    const heading = main.getByRole("heading", { level: 1 });
    await waitVisible(heading, "the dashboard page's title");
    await waitVisible(main.getByText(/^資料時間/).first(), "the 資料時間 line");
    const header = await heading.evaluateHandle((element) => element.closest("header") ?? element.parentElement);
    emit(viewport, "dashboard: arrival from 開啟儀表板 (header)", {
      conversationScrollY,
      headerText: await header.evaluate((element) => element.textContent.replace(/\s+/g, " ").trim().slice(0, 200)),
      refreshButton: await inWindow(main.getByRole("button", { name: "更新資料" })),
      ...(await measure(page, header, { noScroll: true })),
    });
    await shoot(page, viewport, "dashboard-arrival");
    await page.evaluate(() => window.scrollTo(0, 0));
    const dashComponent = main.getByRole("heading", { level: 3, name: "Daily average power" }).locator("xpath=ancestor::section[1]");
    await waitVisible(dashComponent, "Daily average power on the dashboard page");
    const firstDash = main.getByRole("heading", { level: 3 }).first().locator("xpath=ancestor::section[1]");
    await waitVisible(firstDash.locator("canvas").first(), "the dashboard page's first chart");
    const firstPlot = await firstDash.locator("canvas").first().evaluateHandle((element) => element.closest(".h-72") ?? element.parentElement);
    const topOfPage = await measure(page, header, { noScroll: true });
    await page.evaluate(() => window.scrollTo(0, 0));
    emit(viewport, "dashboard: top of page", {
      spans: { titleToFirstPlot: await measureSpan(page, await heading.elementHandle(), firstPlot) },
      ...topOfPage,
    });
    await page.evaluate(() => window.scrollTo(0, 0));
    await shoot(page, viewport, "dashboard-top");
    await main.getByRole("button", { name: "更新資料" }).click();
    await waitVisible(main.getByRole("button", { name: "更新資料" }), "更新資料 to come back after the refresh");
    await waitVisible(dashComponent.locator("canvas").first(), "the dashboard page's chart");
    emit(viewport, "dashboard: Daily average power component", await measure(page, await dashComponent.elementHandle()));
    emit(viewport, "dashboard: tooltip", { tooltip: await tooltip(page, dashComponent) });
    await page.mouse.move(0, 0);
    await openSource(dashComponent, "Daily average power", "dashboard", "dashboard-source-panel", "請 AI 解釋");
  });

  // A dashboard with a KPI (the answer's draft in the preview has none).
  await step("kpi", async () => {
    await page.goto(`${BASE}/dashboards`);
    await waitVisible(main.getByRole("link", { name: "Plant energy overview" }).first(), "Plant energy overview in the list");
    await main.getByRole("link", { name: "Plant energy overview" }).first().click();
    await waitVisible(main.getByRole("heading", { level: 1, name: "Plant energy overview" }), "the Plant energy overview page");
    await page.evaluate(() => window.scrollTo(0, 0));
    const kpiHeading = main.getByRole("heading", { level: 3 }).first();
    await waitVisible(kpiHeading, "the first component");
    const kpi = kpiHeading.locator("xpath=ancestor::section[1]");
    await waitVisible(kpi.getByText("kWh").first(), "the KPI's value");
    emit(viewport, "dashboard: KPI component (Plant energy overview)", {
      kpiText: await kpi.evaluate((element) => element.textContent.replace(/\s+/g, " ").trim().slice(0, 120)),
      ...(await measure(page, await kpi.elementHandle(), { noScroll: true })),
    });
    await shoot(page, viewport, "dashboard-kpi");
    // The runbook clicks the number itself: the KPI's value is a button that opens its source.
    const value = kpi.getByRole("button", { name: /查看來源$/ }).first();
    await waitVisible(value, "the KPI value's 查看來源 button");
    const scrollBefore = await page.evaluate(() => Math.round(window.scrollY));
    await value.click();
    const kpiPanel = kpi.getByRole("region", { name: "資料來源" });
    await waitVisible(kpiPanel, "the KPI's source panel");
    emit(viewport, "dashboard: KPI source panel (opened by clicking the number)", {
      scrollBefore,
      ...(await measure(page, await kpiPanel.elementHandle(), { noScroll: true })),
    });
  });

  // Step 4's follow-up question in the conversation whose answer has the dashboard card (a long
  // thread, as in the demo). `!slow` keeps the question running until 停止.
  await step("follow-up", async () => {
    await page.goto(`${BASE}${PREVIEW_CONVERSATION}`);
    await waitVisible(main.locator("section[data-answer-dashboard]").first(), "the answer's dashboard card");
    const again = main.getByRole("textbox", { name: "繼續問 TrustIoT" });
    await again.fill(`${FOLLOW_UP} !slow`);
    const scrollBefore = await page.evaluate(() => Math.round(window.scrollY));
    await main.getByRole("button", { name: "送出" }).click();
    const pending = main.getByRole("list", { name: "正在回答的問題" });
    const working = pending.getByText("正在查詢讀數…");
    await waitVisible(working, "正在查詢讀數… under the follow-up question");
    emit(viewport, "conversation: follow-up waiting (正在查詢讀數…)", {
      scrollBefore,
      waitingLine: await inWindow(working),
      ...(await measure(page, await pending.elementHandle(), { noScroll: true })),
    });
    await shoot(page, viewport, "conversation-follow-up-waiting");
    const writing = main.getByText("正在寫回答…").first();
    await waitVisible(writing, "正在寫回答… once the first paragraph streamed");
    emit(viewport, "conversation: follow-up streaming (正在寫回答…)", {
      writingLine: await inWindow(writing),
      ...(await measure(page, await pending.elementHandle(), { noScroll: true })),
    });
    await shoot(page, viewport, "conversation-streaming");
    await main.getByRole("button", { name: "停止" }).click();
    const stopped = main.getByText(/^已停止/).first();
    await waitVisible(stopped, "the 已停止 notice after 停止");
    emit(viewport, "conversation: stopped notice", {
      composer: await main.locator(".sticky").last().evaluate((element) => Math.round(element.getBoundingClientRect().height)),
      noticeBox: await stopped.evaluate((element) => ({ width: Math.round(element.getBoundingClientRect().width), height: Math.round(element.getBoundingClientRect().height) })),
      ...(await measure(page, await stopped.elementHandle(), { noScroll: true })),
    });
    await shoot(page, viewport, "conversation-stopped");
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
