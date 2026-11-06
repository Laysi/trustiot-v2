// The render of Laysi/trustiot-v2#1142 (docs/test-reports/2026-11-07-1142-login-email.md):
// opens the sample login email's HTML part in the pinned Playwright Chromium, light and dark,
// wide and narrow, and records what a reader would see. Runs in the container with the work
// directory at /work (sample.html in, out/ for the screenshots and record.json).
import { readFileSync, writeFileSync } from "node:fs";
import { chromium } from "playwright";

const html = readFileSync("/work/sample.html", "utf8");
const browser = await chromium.launch();
const record = { chromium: browser.version(), shots: [] };
for (const scheme of ["light", "dark"]) {
  for (const [name, width] of [
    ["wide", 720],
    ["narrow", 360],
  ]) {
    const context = await browser.newContext({ viewport: { width, height: 640 }, colorScheme: scheme, deviceScaleFactor: 1 });
    const page = await context.newPage();
    const requests = [];
    page.on("request", (request) => requests.push(request.url()));
    await page.setContent(html, { waitUntil: "load" });
    const file = `${scheme}-${name}.png`;
    await page.screenshot({ path: `/work/out/${file}`, fullPage: true });
    const facts = await page.evaluate(() => {
      const code = document.querySelector(".ti-code");
      const style = code ? getComputedStyle(code) : null;
      const box = document.querySelector(".ti-code-box");
      const preheader = document.body.firstElementChild;
      const card = document.querySelector(".ti-card");
      return {
        codeText: code?.textContent ?? null,
        codeColor: style?.color ?? null,
        codeFont: style?.fontFamily ?? null,
        codeFontSize: style?.fontSize ?? null,
        codeLetterSpacing: style?.letterSpacing ?? null,
        codeBoxBackground: box ? getComputedStyle(box).backgroundColor : null,
        cardBackground: card ? getComputedStyle(card).backgroundColor : null,
        cardWidth: card ? Math.round(card.getBoundingClientRect().width) : null,
        pageBackground: getComputedStyle(document.body).backgroundColor,
        preheaderDisplay: preheader ? getComputedStyle(preheader).display : null,
        preheaderStartsWith: preheader?.textContent?.slice(0, 64) ?? null,
        visibleTextStart: document.body.innerText.trim().slice(0, 40),
        horizontalScroll: document.documentElement.scrollWidth > window.innerWidth,
      };
    });
    record.shots.push({ file, scheme, width, requests, ...facts });
    await context.close();
  }
}
await browser.close();
writeFileSync("/work/out/record.json", `${JSON.stringify(record, null, 2)}\n`);
