// Probe of docs/test-reports/2026-11-04-997-custom-page-motion.md: with the motion layer in place, what a comparison at the end
// of the task (a microtask, as runtime.ts `requestCompare`) and in the next animation frame (`poke`)
// reads for a 0 s CSS animation started by :hover or by a class a timer adds, and for a clamped
// animate() (duration, delay and end delay 0, as seal.ts makes them).
import { chromium } from "playwright-core";

const DECLS = "transition-property: none !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-play-state: running !important; animation-timeline: auto !important;";
const PAGE = `<!doctype html><html><head><style>@layer { * { ${DECLS} } }</style></head><body>
<style>
  @keyframes pop { from { opacity: 0.2 } to { opacity: 0.9 } }
  #hov { width: 80px; height: 40px; background: #ccc; }
  #hov:hover { animation: pop 400ms forwards; }
  #cls.on { animation: pop 400ms forwards; }
</style>
<div id="hov">h</div><div id="cls">c</div><div id="w">w</div>
<script>
  window.seen = [];
  const read = (when) => {
    // As the comparison reads: styles first, then document.getAnimations().
    getComputedStyle(document.getElementById("hov")).opacity;
    window.seen.push({ when, animations: document.getAnimations().map((a) => ({
      target: a.effect.target.id, playState: a.playState, pending: a.pending,
      endTime: a.effect.getComputedTiming().endTime, currentTime: a.currentTime, rate: a.playbackRate })) });
  };
  const compare = (why) => { queueMicrotask(() => read(why + ": end of task")); requestAnimationFrame(() => read(why + ": next frame")); };
  addEventListener("pointerover", (e) => { if (e.isTrusted && e.target.id === "hov") compare("hover"); }, true);
  window.startClass = () => setTimeout(() => { document.getElementById("cls").classList.add("on"); compare("class"); }, 0);
  window.startAnimate = () => setTimeout(() => {
    const a = document.getElementById("w").animate([{ opacity: 0.2 }, { opacity: 0.9 }], { duration: 400, fill: "forwards" });
    a.effect.updateTiming({ duration: 0, delay: 0, endDelay: 0 });
    compare("animate");
  }, 0);
</script></body></html>`;

const browser = await chromium.launch({ args: ["--site-per-process"] });
const page = await browser.newPage();
await page.setContent(PAGE);
const box = await page.locator("#hov").boundingBox();
await page.mouse.move(box.x + 5, box.y + 5);
await page.waitForTimeout(300);
await page.evaluate(() => window.startClass());
await page.waitForTimeout(300);
await page.evaluate(() => window.startAnimate());
await page.waitForTimeout(300);
console.log(JSON.stringify({ version: browser.version(), seen: await page.evaluate(() => window.seen) }, null, 1));
await browser.close();
