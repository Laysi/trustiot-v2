// Scroll position on navigation (Laysi/trustiot-v2#1185), run in the Playwright image
// (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`) in Traditional Chinese
// (zh-TW, Asia/Taipei). Prints one JSON line per measurement and exits non-zero when a check fails.
//
// Cases:
// - 1280x600: scroll the conversation to 225 px, save its dashboard card and open the dashboard
//   (the steps of the #1179 walkthrough); the dashboard header must be at or below the top bar;
//   going back must restore 225 px.
// - 1920x1080: the same with the conversation scrolled as deep as it goes.
// - 1920x1080 at 400 px: the same, part of the way down.
// - 1280x400: device list scrolled, then a device's page (a second navigation from a scrolled page).
//
// Usage: docs/test-reports/data/2026-11-07-1185-scroll-reset/run.sh (from the repository root).
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
/**
 * The preview's conversation with an answer and its dashboard card. It exists in the preview of
 * Laysi/trustiot-v2#1175 (branch feat/1175-source-panel, PR #1178), not yet in main, so this
 * check ran on that branch's tree with this change's two files applied (the report says so).
 */
const PREVIEW_CONVERSATION = "/conversations/3e7b1c94-8a52-4d60-b9f3-0c6d2a5e7f18";

const browser = await chromium.launch();
const failures = [];

function emit(step, data) {
  console.log(JSON.stringify({ step, ...data }));
}

function check(ok, message) {
  if (!ok) failures.push(message);
}

async function waitVisible(locator, what) {
  try {
    await locator.waitFor({ state: "visible", timeout: WAIT_MS });
  } catch {
    throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for ${what}`);
  }
}

/** Waits until `read` returns a value `accept` takes, naming what was awaited when it never does. */
async function waitFor(page, what, read, accept) {
  const deadline = Date.now() + WAIT_MS;
  let last;
  while (Date.now() < deadline) {
    last = await page.evaluate(read);
    if (accept(last)) return last;
    await page.waitForTimeout(100);
  }
  throw new Error(`timed out waiting for ${what}; last value ${JSON.stringify(last)}`);
}

async function newPage(width, height) {
  const context = await browser.newContext({
    locale: "zh-TW",
    timezoneId: "Asia/Taipei",
    viewport: { width, height },
    colorScheme: "light",
  });
  return context.newPage();
}

const scrollY = (page) => page.evaluate(() => Math.round(window.scrollY));
const maxScroll = (page) =>
  page.evaluate(() => Math.round(document.documentElement.scrollHeight - window.innerHeight));
const barBottom = (page) =>
  page.evaluate(() => Math.round(document.querySelector("#root header.sticky")?.getBoundingClientRect().bottom ?? 0));

/** Settled: the scroll position has not changed over a few frames. */
async function settledScroll(page) {
  return waitFor(
    page,
    "the scroll position to settle",
    () =>
      new Promise((resolve) => {
        const first = window.scrollY;
        requestAnimationFrame(() => requestAnimationFrame(() => resolve([first, window.scrollY])));
      }),
    ([a, b]) => a === b,
  ).then(([y]) => Math.round(y));
}

/**
 * The first leaf elements (tag and text, no element children) whose top is at or below the top
 * bar's bottom, with their offset from that bottom: what a reader sees at the top of the window.
 * Runs in the page.
 */
function anchors(page) {
  return page.evaluate(() => {
    const bar = Math.round(document.querySelector("#root header.sticky")?.getBoundingClientRect().bottom ?? 0);
    const found = [];
    for (const element of document.querySelectorAll("#root main *")) {
      if (element.childElementCount !== 0) continue;
      const text = element.textContent.replace(/\s+/g, " ").trim();
      if (text === "") continue;
      const rect = element.getBoundingClientRect();
      if (rect.width === 0 || rect.height === 0 || rect.top < bar) continue;
      found.push({ signature: `${element.tagName.toLowerCase()}:${text.slice(0, 60)}`, offset: Math.round(rect.top - bar) });
      if (found.length === 6) break;
    }
    return found;
  });
}

/** Where the first of `candidates` that is on the page now is, relative to the top bar's bottom. */
function anchorNow(page, candidates) {
  return page.evaluate((candidates) => {
    const bar = Math.round(document.querySelector("#root header.sticky")?.getBoundingClientRect().bottom ?? 0);
    const leaves = [...document.querySelectorAll("#root main *")].filter((element) => element.childElementCount === 0);
    for (const [index, candidate] of candidates.entries()) {
      const element = leaves.find(
        (leaf) => `${leaf.tagName.toLowerCase()}:${leaf.textContent.replace(/\s+/g, " ").trim().slice(0, 60)}` === candidate.signature,
      );
      if (element) return { index, signature: candidate.signature, offset: Math.round(element.getBoundingClientRect().top - bar) };
    }
    return null;
  }, candidates);
}

/** Whether something other than the link is at the link's centre (the sticky composer, say). */
async function linkCovered(page, locator) {
  const box = await locator.boundingBox();
  if (!box) return true;
  return page.evaluate(
    ([x, y]) => {
      const hit = document.elementFromPoint(x, y);
      return !(hit && hit.closest("a[href]"));
    },
    [box.x + box.width / 2, box.y + box.height / 2],
  );
}

/** Clicks where a person would: at the link's centre; a covered link is clicked the Playwright way. */
async function clickAsUser(page, locator) {
  const box = await locator.boundingBox();
  if (!box) throw new Error("the link to click has no box");
  const x = box.x + box.width / 2;
  const y = box.y + box.height / 2;
  const covered = await linkCovered(page, locator);
  if (covered) {
    await locator.click();
  } else {
    await page.mouse.click(x, y);
  }
  return covered;
}

async function conversationToDashboard(label, width, height, target) {
  const page = await newPage(width, height);
  const main = page.locator("#root main");
  await page.goto(`${BASE}${PREVIEW_CONVERSATION}`);
  const card = main.locator("section[data-answer-dashboard]").first();
  await waitVisible(card, "the answer's dashboard card").catch(async (error) => {
    emit(`${label}: no card`, { url: page.url(), text: (await main.innerText().catch(() => "")).slice(0, 400) });
    throw error;
  });
  await waitVisible(card.locator("canvas").first(), "a chart in the card");
  const save = card.getByRole("button", { name: "儲存到儀表板列表" });
  await waitVisible(save, "the 儲存到儀表板列表 button");
  await save.click();
  await waitVisible(card.getByText(/已儲存為儀表板列表裡的第 \d+ 版/), "the saved message");
  const open = card.getByRole("link", { name: "開啟儀表板" });
  await waitVisible(open, "the 開啟儀表板 link");

  const max = await maxScroll(page);
  let y = target === "max" ? max : Math.min(target, max);
  await page.evaluate((to) => window.scrollTo(0, to), y);
  // At the very bottom the sticky composer can cover the link, which a person could not click:
  // "as deep as it goes" is then the deepest position at which the link can be clicked.
  while ((await linkCovered(page, open)) && y > 0) {
    y = Math.max(0, y - 20);
    await page.evaluate((to) => window.scrollTo(0, to), y);
  }
  const before = await settledScroll(page);
  const anchorsBefore = await anchors(page);
  const covered = await clickAsUser(page, open);
  const heading = main.getByRole("heading", { level: 1 });
  await waitVisible(heading, "the dashboard page's title");
  await waitVisible(main.getByText(/^資料時間/).first(), "the 資料時間 line");
  const after = await settledScroll(page);
  const headerTop = await heading.evaluate((element) =>
    Math.round((element.closest("header") ?? element).getBoundingClientRect().top),
  );
  const bar = await barBottom(page);
  emit(`${label}: arrival at the dashboard`, {
    viewport: `${width}x${height}`,
    conversationMaxScroll: max,
    conversationScrollBeforeOpen: before,
    dashboardScrollY: after,
    dashboardHeaderTop: headerTop,
    topBarBottom: bar,
    path: new URL(page.url()).pathname,
  });
  check(before > 0, `${label}: the conversation was not scrolled before opening (max ${max})`);
  check(after === 0, `${label}: the dashboard opened at scrollY ${after}, not 0`);
  check(headerTop >= bar, `${label}: the dashboard header top ${headerTop} is above the top bar's bottom ${bar}`);

  await page.goBack();
  await waitVisible(card, "the conversation after going back");
  // Still: the scroll position and the page height have not changed over a second.
  const still = await waitFor(
    page,
    "the conversation's scroll position and height to stop changing",
    () =>
      new Promise((resolve) => {
        const first = [window.scrollY, document.documentElement.scrollHeight];
        setTimeout(() => resolve([first, [window.scrollY, document.documentElement.scrollHeight]]), 1000);
      }),
    ([a, b]) => a[0] === b[0] && a[1] === b[1],
  );
  const restored = Math.round(still[1][0]);
  const anchorAfter = await anchorNow(page, anchorsBefore);
  const anchorOffsetBefore = anchorAfter ? anchorsBefore[anchorAfter.index].offset : null;
  emit(`${label}: back to the conversation`, {
    linkWasCovered: covered,
    scrollYBefore: before,
    scrollYAfterBack: restored,
    pageHeightAfterBack: still[1][1],
    anchor: anchorAfter && { signature: anchorAfter.signature, offsetBefore: anchorOffsetBefore, offsetAfter: anchorAfter.offset },
  });
  check(anchorAfter !== null, `${label}: no element the reader saw before leaving is on the page after going back`);
  check(
    anchorAfter !== null && Math.abs(anchorAfter.offset - anchorOffsetBefore) <= 2,
    `${label}: the anchor "${anchorAfter?.signature}" moved from ${anchorOffsetBefore} to ${anchorAfter?.offset} below the top bar (scrollY ${before} -> ${restored})`,
  );
  await page.context().close();
}

async function deviceListToDevice() {
  const width = 1280;
  const height = 400;
  const page = await newPage(width, height);
  const main = page.locator("#root main");
  await page.goto(`${BASE}/devices`);
  const link = main.getByRole("link").filter({ hasText: /\S/ }).last();
  await waitVisible(link, "a device in the list");
  const max = await maxScroll(page);
  await page.evaluate((to) => window.scrollTo(0, to), max);
  const before = await settledScroll(page);
  const href = await link.getAttribute("href");
  await link.click();
  await page.waitForURL((url) => url.pathname !== "/devices", { timeout: WAIT_MS });
  await waitVisible(main.getByRole("heading", { level: 1 }), "the next page's title");
  const after = await settledScroll(page);
  emit("device list to a device page", {
    viewport: `${width}x${height}`,
    listMaxScroll: max,
    listScrollBeforeClick: before,
    clicked: href,
    path: new URL(page.url()).pathname,
    nextScrollY: after,
  });
  check(before > 0, `device list: the list was not scrolled before the click (max ${max})`);
  check(after === 0, `device list: the next page opened at scrollY ${after}, not 0`);
  await page.goBack();
  await waitVisible(link, "the device list after going back");
  const restored = await waitFor(page, `the list's scroll position to return to ${before}`, () => Math.round(window.scrollY), (v) => v === before).catch(
    async () => scrollY(page),
  );
  emit("device list: back", { scrollYBefore: before, scrollYAfterBack: restored });
  check(restored === before, `device list: back restored scrollY ${restored}, expected ${before}`);
  await page.context().close();
}

for (const [name, run] of [
  ["1280x600, 225 px", () => conversationToDashboard("1280x600 at 225 px", 1280, 600, 225)],
  ["1920x1080, as deep as it goes", () => conversationToDashboard("1920x1080 at max", 1920, 1080, "max")],
  ["1920x1080, 400 px", () => conversationToDashboard("1920x1080 at 400 px", 1920, 1080, 400)],
  ["device list", deviceListToDevice],
]) {
  try {
    await run();
  } catch (error) {
    failures.push(`${name}: ${error.message.split("\n")[0]}`);
  }
}
await browser.close();
emit("result", { failures });
if (failures.length > 0) process.exit(1);
