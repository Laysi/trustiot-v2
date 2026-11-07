// The browser check of Laysi/trustiot-v2#1182 (the question being answered stays between the top bar
// and the composer, and its answer is followed while it is written), run in the Playwright image
// (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`), in Traditional Chinese
// (zh-TW, Asia/Taipei). Adapted from the projector walkthrough's `follow-up` step
// (docs/test-reports/data/2026-11-07-1179-projector-walkthrough/browser-check.mjs): the same
// preview emulation and band measurement, plus a frame sampler and an answer-stream tap.
//
// Per viewport it prints JSON lines of DOM numbers:
// - `follow-up: thread`: the preview's conversation with an answer, made longer than the window by
//   asking it a few questions first (each answered by the preview after 2.5 s), then scrolled to
//   its end as a presenter would. (The walkthrough used a conversation with a dashboard card that
//   the preview has only with PR Laysi/trustiot-v2#1178, which main does not have yet.)
// - `follow-up: appeared`: a follow-up sent at the bottom of that long conversation; every
//   animation frame while 正在查詢讀數… is on the page is sampled, and the record is the first
//   sample and the last one (after the page stopped scrolling): the pending turn's box, the
//   waiting line's box, the band, and whether the line is fully inside the band.
// - `follow-up: first paragraph`: the preview streams the answer's first paragraph; its box once
//   the page stopped scrolling.
// - `follow-up: grew N`: paragraphs added to the answer stream one by one (the tap below), each
//   measured once the page stopped scrolling: the newest paragraph and 正在寫回答… in the band.
// - `follow-up: scrolled up`: the reader turns the mouse wheel up; another paragraph arrives; the
//   page must stay where the reader left it.
// - `follow-up: back at the bottom`: the reader turns the wheel down to the end; another
//   paragraph arrives and must be followed again.
// - `new conversation: appeared`: a question sent from home; the same sampling on the new
//   conversation's page.
//
// The preview streams only the first paragraph of its example answer and, with `!slow`, nothing
// after it. So that the answer grows as a real one does, an init script wraps the page's `fetch`
// (the preview installs its own; the wrapper wraps whatever is installed) and, for the answer
// stream (`.../runs/{id}/stream`), passes every event through while keeping the answer's end
// offset, so `__appendParagraph` can add a `text` event at that offset: the app receives and draws
// it like any other paragraph (useAnswerStream, StreamingAnswer).
//
// Waits are for conditions with a 60 s bound whose failure names what was awaited. "The page
// stopped scrolling" is a condition too: the scroll position unchanged for SETTLE_FRAMES animation
// frames in a row. The one time-based check is the scrolled-up case, where the page must NOT move:
// after the paragraph is drawn, the position must stay unchanged for STILL_MS, longer than the
// page's wheel quiet period (200 ms) plus a smooth scroll.
//
// Screenshots are taken only when SHOTS=1 and are printed base64 after the JSON lines, between
// markers, for the author's eyes; they are never part of the committed record.
//
// Usage: docs/test-reports/data/2026-11-07-1182-follow-up-scroll/run.sh (from the repository root).
// VIEWPORTS=<id,id> limits the run to some viewports.
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
const SETTLE_FRAMES = 20;
const STILL_MS = 1500;
/** The preview's conversation with an answer (src/test/fake-conversations.ts, seedPreviewConversations). */
const PREVIEW_CONVERSATION_TITLE = "Line 2 energy this week";
/** Earlier questions asked first, so the thread is longer than the window as in the demo. */
const EARLIER = (n) => `Round ${n}: how did line 2 do on day ${n}?`;
/** The runbook's main request sentence (docs/runbooks/demo-presentation.md, 步驟 3). */
const QUESTION = "做一個儀表板：最近 7 天每天的平均溫度與最高溫度，用長條圖比較，另外用一個數字顯示現在的溫度和濕度。以台北時間分天。";
/** Its change sentence (步驟 4); `!slow` keeps the preview's question running until 停止. */
const FOLLOW_UP = "改成每小時，只看濕度。";
const PARAGRAPH = (n) =>
  `Paragraph ${n} of the fictional answer: humidity in cold-room-1 stayed between 61 % and 68 % ` +
  `during the last 24 hours, with the highest hourly average at 03:00 and the lowest at 15:00. ` +
  `Chiller-2 reported no humidity readings in the same period.`;

const ALL = [
  { id: "1920x1080", width: 1920, height: 1080, dsf: 1, scheme: "light" },
  { id: "1280x720", width: 1280, height: 720, dsf: 1, scheme: "light" },
  { id: "853x400", width: 853, height: 400, dsf: 1.5, scheme: "light" },
  { id: "1280x720-dark", width: 1280, height: 720, dsf: 1, scheme: "dark" },
];
const only = (process.env.VIEWPORTS ?? "").split(",").filter(Boolean);
const VIEWPORTS = only.length > 0 ? ALL.filter((viewport) => only.includes(viewport.id)) : ALL;
const SHOTS = process.env.SHOTS === "1";

const browser = await chromium.launch();
const failures = [];
const consoleErrors = [];
const shots = [];

function emit(viewport, step, data) {
  const where = viewport ? { viewport: viewport.id, css: `${viewport.width}x${viewport.height}`, dsf: viewport.dsf, scheme: viewport.scheme } : {};
  console.log(JSON.stringify({ step, ...where, ...data, consoleErrors: [...consoleErrors] }));
  consoleErrors.length = 0;
}

async function shoot(page, viewport, name) {
  if (!SHOTS) return;
  const image = await page.screenshot({ type: "jpeg", quality: 55, scale: "css" });
  shots.push({ name: `${viewport.id}--${name}`, base64: image.toString("base64") });
}

/** Waits for `locator` to be visible, failing with a message that names what was awaited. */
async function waitVisible(locator, what) {
  try {
    await locator.waitFor({ state: "visible", timeout: WAIT_MS });
  } catch {
    throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for ${what}`);
  }
}

/** Waits for `predicate` (run in the page) to be true, failing with a message that names what was awaited. */
async function waitFor(page, predicate, arg, what) {
  try {
    await page.waitForFunction(predicate, arg, { timeout: WAIT_MS, polling: "raf" });
  } catch {
    throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for ${what}`);
  }
}

/** Waits until the page's scroll position stayed the same for SETTLE_FRAMES frames in a row. */
async function settled(page, what) {
  const result = await page.evaluate(([frames, bound]) => window.__settle(frames, bound), [SETTLE_FRAMES, WAIT_MS]);
  if (!result.settled) throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for the page to stop scrolling (${what})`);
  return result.scrollY;
}

/**
 * Runs in every page before the app.
 * - Hides what the UI preview draws and the product does not, as the walkthrough did
 *   (its previewEmulation): the preview toolbar, the conversation page's draft-list alert, the
 *   composer's completion hint and template button, the home page's setup checklist and suggestions.
 * - `__band()`: the band between the bottom of the sticky top bar and the top of the composer
 *   (the window's bottom where there is no composer), with the scroll-padding the page publishes.
 * - `__sampler`: records, every animation frame, the pending turn and its status line.
 * - `__settle(frames, bound)`: resolves once scrollY is unchanged for `frames` frames.
 * - the answer-stream tap and `__appendParagraph(text)` (see the header).
 */
function pageHelpers() {
  // Preview emulation.
  const hide = () => {
    for (const alert of document.querySelectorAll("[data-slot=alert]")) {
      if (alert.textContent.startsWith("讀不到這段對話的草稿")) alert.setAttribute("data-check-hidden", "draft-list-alert");
    }
    for (const box of document.querySelectorAll("textarea[aria-describedby]")) {
      for (const id of box.getAttribute("aria-describedby").split(/\s+/)) {
        const hint = document.getElementById(id);
        if (!hint) continue;
        for (const node of hint.childNodes) {
          if (node.nodeType === 3 && node.nodeValue.includes("灰字")) node.nodeValue = "";
        }
      }
    }
    const heading = [...document.querySelectorAll("#root main h1")].find((h) => h.textContent.trim() === "首頁");
    const grid = heading?.closest("section")?.parentElement;
    if (grid) {
      for (const child of grid.children) {
        if (child.textContent.includes("開始使用 TrustIoT") || child.textContent.includes("試試這些問題")) child.setAttribute("data-check-hidden", "home-slot");
      }
    }
  };
  document.addEventListener("DOMContentLoaded", () => {
    const style = document.createElement("style");
    style.textContent = [
      "#ui-preview-toolbar{display:none!important}",
      "[data-check-hidden]{display:none!important}",
      'button[aria-label="問句範本（/）"]{display:none!important}',
    ].join("\n");
    document.head.append(style);
    new MutationObserver(hide).observe(document.body, { subtree: true, childList: true, characterData: true });
    hide();
  });

  // Measurements.
  const box = (element) => {
    if (!element) return null;
    const rect = element.getBoundingClientRect();
    return { top: Math.round(rect.top), bottom: Math.round(rect.bottom), height: Math.round(rect.height) };
  };
  const composerBar = () =>
    [...document.querySelectorAll("#root main .sticky")].find((element) => element.querySelector("textarea")) ?? null;
  const band = () => {
    const vh = document.documentElement.clientHeight;
    const header = document.querySelector("#root header.sticky");
    const composer = composerBar();
    const top = header ? header.getBoundingClientRect().bottom : 0;
    const bottom = composer ? Math.min(vh, composer.getBoundingClientRect().top) : vh;
    const style = getComputedStyle(document.documentElement);
    return {
      top: Math.round(top),
      bottom: Math.round(bottom),
      height: Math.round(bottom - top),
      window: vh,
      header: header ? Math.round(header.getBoundingClientRect().height) : 0,
      composer: composer ? Math.round(composer.getBoundingClientRect().height) : 0,
      scrollPadding: { top: style.scrollPaddingTop, bottom: style.scrollPaddingBottom },
    };
  };
  const inBand = (element, b) => {
    if (!element) return null;
    const rect = element.getBoundingClientRect();
    return rect.top >= b.top - 0.5 && rect.bottom <= b.bottom + 0.5;
  };
  const pending = () => document.querySelector('#root main ol[aria-label="正在回答的問題"]');
  const statusLine = (text) => [...(pending()?.querySelectorAll("[role=status]") ?? [])].find((element) => element.textContent.includes(text)) ?? null;
  const newestParagraph = () => {
    const article = pending()?.querySelector("article");
    if (!article) return null;
    const blocks = [...article.querySelectorAll("p, li, table, h1, h2, h3, h4, pre, blockquote")].filter((element) => element.getAttribute("role") !== "status");
    return blocks.at(-1) ?? null;
  };
  window.__band = band;
  window.__look = () => {
    const b = band();
    const waiting = statusLine("正在查詢讀數");
    const writing = statusLine("正在寫回答");
    const newest = newestParagraph();
    return {
      scrollY: Math.round(window.scrollY),
      scrollHeight: document.documentElement.scrollHeight,
      band: b,
      pending: box(pending()),
      waitingLine: waiting ? { ...box(waiting), inBand: inBand(waiting, b) } : null,
      newestParagraph: newest ? { ...box(newest), inBand: inBand(newest, b), text: newest.textContent.slice(0, 40) } : null,
      writingLine: writing ? { ...box(writing), inBand: inBand(writing, b) } : null,
    };
  };
  window.__sampler = {
    samples: [],
    running: false,
    start() {
      this.samples = [];
      this.running = true;
      const t0 = performance.now();
      const tick = () => {
        if (!this.running) return;
        const waiting = statusLine("正在查詢讀數");
        if (waiting) this.samples.push({ ms: Math.round(performance.now() - t0), ...window.__look() });
        requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    },
    stop() {
      this.running = false;
      return this.samples;
    },
  };
  window.__settle = (frames, bound) =>
    new Promise((resolve) => {
      const deadline = performance.now() + bound;
      let last = window.scrollY;
      let same = 0;
      const tick = () => {
        const now = window.scrollY;
        same = now === last ? same + 1 : 0;
        last = now;
        if (same >= frames) resolve({ settled: true, scrollY: Math.round(now) });
        else if (performance.now() > deadline) resolve({ settled: false });
        else requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    });
  window.__still = (ms) =>
    new Promise((resolve) => {
      const start = window.scrollY;
      const t0 = performance.now();
      const moves = [];
      const tick = () => {
        if (window.scrollY !== start) moves.push(Math.round(window.scrollY));
        if (performance.now() - t0 >= ms) resolve({ from: Math.round(start), to: Math.round(window.scrollY), moves: moves.slice(0, 10) });
        else requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    });

  // The answer-stream tap.
  const taps = [];
  const encoder = new TextEncoder();
  const wrap = (base) =>
    async function tappedFetch(input, init) {
      const response = await base(input, init);
      const url = typeof input === "string" ? input : input instanceof Request ? input.url : String(input);
      if (!/\/runs\/[^/]+\/stream$/.test(new URL(url, location.href).pathname) || !response.body) return response;
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      const tap = { end: 0, buffer: "", controller: null, open: true };
      const body = new ReadableStream({
        start(controller) {
          tap.controller = controller;
          taps.push(tap);
          (async () => {
            try {
              for (;;) {
                const { done, value } = await reader.read();
                if (done) break;
                tap.buffer += decoder.decode(value, { stream: true });
                for (let at = tap.buffer.indexOf("\n\n"); at >= 0; at = tap.buffer.indexOf("\n\n")) {
                  const frame = tap.buffer.slice(0, at);
                  tap.buffer = tap.buffer.slice(at + 2);
                  const data = /^data: (.*)$/m.exec(frame);
                  if (/^event: text$/m.test(frame) && data) {
                    const piece = JSON.parse(data[1]);
                    tap.end = Math.max(tap.end, piece.offset + piece.text.length);
                  }
                }
                controller.enqueue(value);
              }
              tap.open = false;
              controller.close();
            } catch (error) {
              tap.open = false;
              try {
                controller.error(error);
              } catch {
                // Already cancelled.
              }
            }
          })();
        },
        cancel(reason) {
          tap.open = false;
          return reader.cancel(reason);
        },
      });
      return new Response(body, { status: response.status, statusText: response.statusText, headers: response.headers });
    };
  let current = wrap(window.fetch.bind(window));
  Object.defineProperty(window, "fetch", {
    configurable: true,
    get: () => current,
    set: (value) => {
      current = wrap(value);
    },
  });
  window.__appendParagraph = (text) => {
    const tap = taps.filter((candidate) => candidate.open && candidate.buffer === "").at(-1);
    if (!tap) return false;
    const piece = { offset: tap.end, text: `\n\n${text}` };
    tap.end += piece.text.length;
    tap.controller.enqueue(encoder.encode(`event: text\nid: ${tap.end}\ndata: ${JSON.stringify(piece)}\n\n`));
    return true;
  };
}

/** The first and the last sample of the waiting line, and how many frames had it. */
function summarizeSamples(samples) {
  if (samples.length === 0) return { frames: 0 };
  const last = samples.at(-1);
  const lastY = last.scrollY;
  // Frames at the end with the final scroll position: the page had stopped scrolling.
  let still = 0;
  for (let i = samples.length - 1; i >= 0 && samples[i].scrollY === lastY; i -= 1) still += 1;
  return { frames: samples.length, first: samples[0], last, stillFramesAtEnd: still };
}

async function appendParagraph(page, n) {
  const added = await page.evaluate((text) => window.__appendParagraph(text), PARAGRAPH(n));
  if (!added) throw new Error(`no open answer stream to add paragraph ${n} to`);
  await waitFor(
    page,
    (marker) => [...document.querySelectorAll('#root main ol[aria-label="正在回答的問題"] article p')].some((p) => p.textContent.startsWith(marker)),
    `Paragraph ${n} of`,
    `paragraph ${n} to be drawn in the answer being written`,
  );
}

async function walk(viewport) {
  const context = await browser.newContext({
    locale: "zh-TW",
    timezoneId: "Asia/Taipei",
    viewport: { width: viewport.width, height: viewport.height },
    deviceScaleFactor: viewport.dsf,
    colorScheme: viewport.scheme,
  });
  await context.addInitScript(pageHelpers);
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

  await page.goto(`${BASE}/`);
  await waitVisible(page.getByRole("heading", { level: 1, name: "首頁" }), "the home page's heading 首頁");
  emit(viewport, "language", {
    lang: await page.evaluate(() => document.documentElement.lang),
    languages: await page.evaluate(() => navigator.languages),
    timeZone: await page.evaluate(() => Intl.DateTimeFormat().resolvedOptions().timeZone),
    theme: await page.evaluate(() => document.documentElement.dataset.theme ?? null),
  });

  // The follow-up at the bottom of the preview's long conversation (the runbook's step 4).
  await step("follow-up", async () => {
    await page.goto(`${BASE}/conversations`);
    const link = main.getByRole("link", { name: new RegExp(PREVIEW_CONVERSATION_TITLE) }).first();
    await waitVisible(link, `${PREVIEW_CONVERSATION_TITLE} in the conversation list`);
    await link.click();
    const box = main.getByRole("textbox", { name: "繼續問 TrustIoT" });
    await waitVisible(box, "the conversation's composer 繼續問 TrustIoT");
    await waitVisible(main.getByRole("article", { name: "TrustIoT 的回答" }).first(), "the conversation's answer");
    // A thread longer than the window: earlier questions, each answered before the next.
    let rounds = 0;
    while (rounds < 8 && (await page.evaluate(() => document.documentElement.scrollHeight - window.innerHeight)) < 600) {
      rounds += 1;
      const answers = await main.getByRole("article", { name: "TrustIoT 的回答" }).count();
      await box.fill(EARLIER(rounds));
      await main.getByRole("button", { name: "送出" }).click();
      await waitFor(
        page,
        (count) =>
          document.querySelectorAll('#root main article[aria-label="TrustIoT 的回答"]').length > count &&
          !document.querySelector('#root main ol[aria-label="正在回答的問題"]'),
        answers,
        `the answer to earlier question ${rounds}`,
      );
    }
    // A presenter scrolls to the end of the conversation before asking.
    await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
    const scrollBefore = await settled(page, "the scroll to the end of the conversation");
    emit(viewport, "follow-up: thread", {
      earlierQuestions: rounds,
      answers: await main.getByRole("article", { name: "TrustIoT 的回答" }).count(),
      scrollHeight: await page.evaluate(() => document.documentElement.scrollHeight),
      scrollBefore,
    });
    await box.fill(`${FOLLOW_UP} !slow`);
    await page.evaluate(() => window.__sampler.start());
    await main.getByRole("button", { name: "送出" }).click();
    const pending = main.getByRole("list", { name: "正在回答的問題" });
    await waitVisible(pending, "the list 正在回答的問題 after sending the follow-up");
    // The preview streams the first paragraph halfway through its 2.5 s; until then the line shows.
    await waitVisible(pending.getByText("正在寫回答…"), "正在寫回答… once the first paragraph streamed");
    const samples = await page.evaluate(() => window.__sampler.stop());
    emit(viewport, "follow-up: appeared (正在查詢讀數…)", { scrollBefore, ...summarizeSamples(samples) });

    await settled(page, "following the first paragraph");
    emit(viewport, "follow-up: first paragraph", await page.evaluate(() => window.__look()));
    await shoot(page, viewport, "follow-up-first-paragraph");

    // The answer grows while the reader stays at the bottom.
    let n = 2;
    for (; n <= 4; n += 1) {
      await appendParagraph(page, n);
      await settled(page, `following paragraph ${n}`);
      emit(viewport, `follow-up: grew (paragraph ${n})`, await page.evaluate(() => window.__look()));
    }
    await shoot(page, viewport, "follow-up-grew");

    // The reader turns the wheel up: the page stops following.
    const b = await page.evaluate(() => window.__band());
    await page.mouse.move(viewport.width / 2, b.top + Math.max(10, b.height / 2));
    await page.mouse.wheel(0, -300);
    const upAt = await settled(page, "the reader's wheel up");
    const beforeAppend = await page.evaluate(() => window.__look());
    await appendParagraph(page, n);
    const still = await page.evaluate((ms) => window.__still(ms), STILL_MS);
    const afterAppend = await page.evaluate(() => window.__look());
    emit(viewport, `follow-up: scrolled up, paragraph ${n} arrived`, {
      scrolledUpTo: upAt,
      newestBefore: beforeAppend.newestParagraph,
      stayedPut: still.from === upAt && still.to === upAt && still.moves.length === 0,
      still,
      after: afterAppend,
    });
    await shoot(page, viewport, "follow-up-scrolled-up");
    n += 1;

    // Back to the bottom with the wheel: the page follows again.
    for (let turn = 0; turn < 20; turn += 1) {
      const atBottom = await page.evaluate(() => {
        const look = window.__look();
        return look.pending.bottom <= look.band.bottom + 32;
      });
      if (atBottom) break;
      await page.mouse.wheel(0, 600);
      await settled(page, "the reader's wheel down");
    }
    const backAt = await page.evaluate(() => window.__look());
    await appendParagraph(page, n);
    await settled(page, `following paragraph ${n} after coming back`);
    emit(viewport, `follow-up: back at the bottom, paragraph ${n} arrived`, {
      backAt: { scrollY: backAt.scrollY, pendingBottom: backAt.pending.bottom, bandBottom: backAt.band.bottom },
      after: await page.evaluate(() => window.__look()),
    });
    await shoot(page, viewport, "follow-up-back");

    await main.getByRole("button", { name: "停止" }).click();
    await waitVisible(main.getByText(/^已停止/).first(), "the 已停止 notice after 停止");
  });

  // A question sent from home: the new conversation's page.
  await step("new conversation", async () => {
    await page.goto(`${BASE}/`);
    const box = main.getByRole("textbox", { name: "問 TrustIoT" });
    await waitVisible(box, "the home composer 問 TrustIoT");
    await box.fill(`${QUESTION} !slow`);
    // The page changes inside the app (no reload), so the sampler started here keeps sampling.
    await page.evaluate(() => window.__sampler.start());
    await main.getByRole("button", { name: "送出" }).click();
    try {
      await page.waitForURL(/\/conversations\/[0-9a-f-]{36}$/, { timeout: WAIT_MS });
    } catch {
      throw new Error("timed out after 60 s waiting for the new conversation's page");
    }
    const pending = main.getByRole("list", { name: "正在回答的問題" });
    await waitVisible(pending.getByText("正在寫回答…"), "正在寫回答… on the new conversation's page");
    const samples = (await page.evaluate(() => window.__sampler.stop())).filter((sample) => sample.pending !== null);
    emit(viewport, "new conversation: appeared (正在查詢讀數…)", summarizeSamples(samples));
    await settled(page, "the new conversation's first paragraph");
    emit(viewport, "new conversation: first paragraph", await page.evaluate(() => window.__look()));
    await shoot(page, viewport, "new-conversation");
    await main.getByRole("button", { name: "停止" }).click();
    await waitVisible(main.getByText(/^已停止/).first(), "the 已停止 notice after 停止");
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
process.exitCode = failures.length > 0 ? 1 : 0;
