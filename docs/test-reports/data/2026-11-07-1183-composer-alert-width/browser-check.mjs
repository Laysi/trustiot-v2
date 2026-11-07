// Measures the conversation composer's two alerts (Laysi/trustiot-v2#1183), the "stopped" notice
// and the send-failure alert, in the Playwright image (frontend/e2e/image.ts) against the UI
// preview, in zh-TW at 1280x720 and 853x400. Every state prints one JSON line: the alert's box,
// its number of lines, the text column's width, and the height of the sticky composer wrapper.
// Run with run.sh from the repository root; LABEL names the run (before or after the fix).
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5174";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 60_000;
const QUESTION = "最近 7 天每天的平均溫度是多少？";
const VIEWPORTS = [
  { id: "1280x720", width: 1280, height: 720 },
  { id: "853x400", width: 853, height: 400 },
];

async function waitVisible(locator, what) {
  try {
    await locator.waitFor({ state: "visible", timeout: WAIT_MS });
  } catch {
    throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for ${what}`);
  }
}

/** The alert's box, its text column, its line count and the sticky wrapper's height. */
function boxOf(alert) {
  return alert.evaluate((element) => {
    const rect = element.getBoundingClientRect();
    const text = element.querySelector("[data-slot=alert-description]");
    const textRect = text.getBoundingClientRect();
    const paragraph = text.querySelector("p") ?? text;
    const lineHeight = Number.parseFloat(getComputedStyle(paragraph).lineHeight) || 20;
    const wrapper = element.closest(".sticky");
    return {
      alert: { width: Math.round(rect.width), height: Math.round(rect.height) },
      textColumn: { width: Math.round(textRect.width), height: Math.round(textRect.height) },
      lines: Math.round(paragraph.getBoundingClientRect().height / lineHeight),
      gridColumns: getComputedStyle(element).gridTemplateColumns,
      composerWrapperHeight: wrapper ? Math.round(wrapper.getBoundingClientRect().height) : null,
      text: element.textContent.replace(/\s+/g, " ").trim().slice(0, 60),
    };
  });
}

const browser = await chromium.launch();
const failures = [];
for (const viewport of VIEWPORTS) {
  const context = await browser.newContext({ locale: "zh-TW", timezoneId: "Asia/Taipei", viewport: { width: viewport.width, height: viewport.height } });
  const page = await context.newPage();
  const main = page.locator("#root main");
  const out = (state, data) => console.log(JSON.stringify({ label: process.env.LABEL, viewport: viewport.id, state, ...data }));
  try {
    // A conversation with an answer: ask from the home page, as the walkthrough does.
    await page.goto(`${BASE}/`);
    const home = main.getByRole("textbox", { name: "問 TrustIoT" });
    await waitVisible(home, "the home composer 問 TrustIoT");
    await home.fill(QUESTION);
    await main.getByRole("button", { name: "送出" }).click();
    try {
      await page.waitForURL(/\/conversations\/[0-9a-f-]{36}$/, { timeout: WAIT_MS });
    } catch {
      throw new Error("timed out after 60 s waiting for the new conversation's page");
    }
    await waitVisible(main.getByRole("article", { name: "TrustIoT 的回答" }).last(), "the answer to the first question");
    const box = main.getByRole("textbox", { name: "繼續問 TrustIoT" });
    await waitVisible(box, "the follow-up question box");

    // Stopped: a `!slow` question keeps running until 停止.
    await box.fill("改成每小時，只看濕度。 !slow");
    await main.getByRole("button", { name: "送出" }).click();
    await waitVisible(main.getByText("正在寫回答…").first(), "正在寫回答… once the first paragraph streamed");
    await main.getByRole("button", { name: "停止" }).click();
    const stopped = main.locator("[data-slot=alert][role=status]").filter({ hasText: /^已停止/ });
    await waitVisible(stopped, "the 已停止 notice after 停止");
    out("stopped", await boxOf(stopped));

    // Send failure: the preview toolbar fails the next request.
    await page.getByLabel("Fail next request").selectOption("unavailable");
    await main.getByRole("textbox", { name: "繼續問 TrustIoT" }).fill("再試一次");
    await main.getByRole("button", { name: "送出" }).click();
    const failed = main.locator("[data-slot=alert]").filter({ hasText: /問題沒有送出/ });
    await waitVisible(failed, "the 問題沒有送出 alert");
    out("send failed", await boxOf(failed));
  } catch (error) {
    failures.push(`${viewport.id}: ${error.message.split("\n")[0]}`);
    out("FAILED", { error: error.message.split("\n")[0], url: page.url(), body: (await page.locator("body").innerText()).replace(/\s+/g, " ").slice(0, 300) });
  }
  await context.close();
}
await browser.close();
console.log(JSON.stringify({ label: process.env.LABEL, summary: true, failures }));
process.exitCode = failures.length > 0 ? 1 : 0;
