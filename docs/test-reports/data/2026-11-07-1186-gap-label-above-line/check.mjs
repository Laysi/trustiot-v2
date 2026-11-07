// Browser check of the gap labels on the device history chart (Laysi/trustiot-v2#1186), in the
// Playwright image against the UI preview, zh-TW, Asia/Taipei. Screenshots are printed base64.
// CASE=repro: chiller-2 over 7 days (the issue's reproduction) and boiler-1 (stalled 45 minutes)
// over 24 hours (a narrow gap at the end of the range). CASE=demo: cold-room-1 over 1 hour with the
// readings every minute and none for the last five (the demo runbook's step 2; run.sh patches the
// preview's fixture for it in the copy it runs).
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL;
const LABEL = process.env.LABEL ?? "after";
const CASE = process.env.CASE ?? "repro";
const WAIT_MS = 60_000;
const VIEWPORTS = [
  { id: "853x400", width: 853, height: 400 },
  { id: "1280x720", width: 1280, height: 720 },
];

const browser = await chromium.launch();
const shots = [];

for (const viewport of VIEWPORTS) {
  const context = await browser.newContext({
    locale: "zh-TW",
    timezoneId: "Asia/Taipei",
    viewport: { width: viewport.width, height: viewport.height },
    colorScheme: "light",
  });
  const page = await context.newPage();
  const main = page.locator("#root main");
  const history = main.locator("section[aria-labelledby=device-history-title]");
  const say = (data) => console.log(JSON.stringify({ label: LABEL, viewport: viewport.id, ...data }));

  const openDevice = async (name) => {
    await page.goto(`${BASE}/devices`);
    await main.getByRole("link", { name }).first().waitFor({ state: "visible", timeout: WAIT_MS });
    await main.getByRole("link", { name }).first().click();
    await main.getByRole("heading", { name: "目前讀數" }).waitFor({ state: "visible", timeout: WAIT_MS });
    await history.locator("canvas").first().waitFor({ state: "visible", timeout: WAIT_MS });
  };
  const pick = async (range) => {
    await history.getByRole("button", { name: range, exact: true }).click();
    await page.waitForFunction(
      (range) =>
        document
          .querySelector("section[aria-labelledby=device-history-title] fieldset [aria-pressed=true]")
          ?.textContent?.includes(range),
      range,
      { timeout: WAIT_MS },
    );
    await history.locator("canvas").first().waitFor({ state: "visible", timeout: WAIT_MS });
    await page.waitForTimeout(1500);
  };
  /** The chart itself, centered in the window so the sticky header does not cover it. */
  const shoot = async (name) => {
    const plot = history.locator("canvas").first();
    await plot.evaluate((element) => element.scrollIntoView({ block: "center" }));
    await page.waitForTimeout(300);
    const box = await plot.boundingBox();
    const image = await page.screenshot({
      type: "jpeg",
      quality: 85,
      clip: { x: Math.max(0, box.x - 8), y: Math.max(0, box.y - 8), width: box.width + 16, height: Math.min(box.height + 16, viewport.height) },
    });
    shots.push({ name: `${LABEL}-${viewport.id}-${name}`, base64: image.toString("base64") });
    say({ shot: name, canvas: `${Math.round(box.width)}x${Math.round(box.height)}` });
  };
  /** The drawn width of a label's chip: 12 px, weight 600, 7 px padding and a 1 px border each side. */
  const chipWidth = (text) =>
    page.evaluate((text) => {
      const family = getComputedStyle(document.body).fontFamily;
      const context = document.createElement("canvas").getContext("2d");
      context.font = `600 12px ${family}`;
      return Math.round(context.measureText(text).width + 2 * 7 + 2);
    }, text);
  /** The gap texts the card says in words (the chart's own are only on the canvas). */
  const cardGaps = () =>
    history.evaluate((element) => element.textContent.match(/[0-9,.]+ ?(分鐘|小時|天)[^。]*?沒有讀數/g) ?? []);

  if (CASE === "repro") {
    await openDevice("chiller-2");
    await pick("7 天");
    await shoot("chiller-2-7d");
    say({ case: "chiller-2 7d", card: await cardGaps() });
    for (const text of ["5 小時 45 分鐘沒有讀數", "3 天 23 小時沒有讀數"]) say({ text, chip: await chipWidth(text) });

    await openDevice("boiler-1");
    await pick("24 小時");
    await shoot("boiler-1-24h");
    say({ case: "boiler-1 24h", card: await cardGaps() });
  } else {
    await openDevice("cold-room-1");
    await pick("1 小時");
    await shoot("cold-room-1-1h");
    say({ case: "cold-room-1 1h", card: await cardGaps(), chip4: await chipWidth("4 分鐘沒有讀數"), chip5: await chipWidth("5 分鐘沒有讀數") });
  }
  await context.close();
}

await browser.close();
console.log("=== SCREENSHOTS BEGIN ===");
for (const shot of shots) console.log(`SHOT ${shot.name} ${shot.base64}`);
console.log("=== SCREENSHOTS END ===");
