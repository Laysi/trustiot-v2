// Probe of docs/test-reports/2026-11-04-997-custom-page-motion.md: can ::first-letter, ::first-line, ::selection and
// ::placeholder be animated or transitioned in Chromium, and what does a rule that only sets
// motion properties on them change in the document?
import { chromium } from "playwright-core";

const browser = await chromium.launch({ args: ["--site-per-process"] });
const page = await browser.newPage();
const out = { version: browser.version() };
await page.setContent(`<!doctype html><html><head><style id="probe"></style></head><body>
<style>
  @keyframes fade { from { color: red } to { color: blue } }
  .a::first-letter { animation: fade 3s infinite; }
  .b::first-line { animation: fade 3s infinite; }
  .c::selection { animation: fade 3s infinite; }
  input::placeholder { animation: fade 3s infinite; }
  .t::first-letter { color: black; transition: color 2s; }
  .t.on::first-letter { color: red; }
  .u::first-line { color: black; transition: color 2s; }
  .u.on::first-line { color: red; }
</style>
<p class="a">Alpha</p><p class="b">Beta</p><p class="c" id="c">Gamma</p><input placeholder="ph">
<p class="t" id="t">Tau</p><p class="u" id="u">Upsilon</p>
<p id="kpi" style="font: 28px system-ui">本週<b id="week" style="display:inline-block; rotate: -10deg">690.0</b>kWh</p>
</body></html>`);
await page.evaluate(() => {
  const range = document.createRange();
  range.selectNodeContents(document.getElementById("c"));
  getSelection().addRange(range);
});
await page.waitForTimeout(100);
await page.evaluate(() => { document.getElementById("t").classList.add("on"); document.getElementById("u").classList.add("on"); });
await page.waitForTimeout(50);
out.animations = await page.evaluate(() =>
  document.getAnimations().map((a) => [a.constructor.name, a.effect?.target?.className || a.effect?.target?.localName, a.effect?.pseudoElement, a.playState]),
);
const boxes = () =>
  page.evaluate(() => {
    const text = document.getElementById("week").firstChild;
    const range = document.createRange();
    range.selectNodeContents(text);
    const rects = [...range.getClientRects()].map((r) => [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)]);
    const fl = getComputedStyle(document.getElementById("week"), "::first-letter");
    return { rects, firstLetterRotate: fl.rotate, firstLetterFontSize: fl.fontSize, ownFontSize: getComputedStyle(document.getElementById("week")).fontSize };
  });
out.before = await boxes();
await page.evaluate(() => {
  document.getElementById("probe").textContent = "@layer { ::first-letter { animation-duration: 0s !important } ::first-line { animation-duration: 0s !important } }";
});
out.withFirstLetterRules = await boxes();
await page.evaluate(() => {
  document.getElementById("probe").textContent = "@layer { ::selection { animation-duration: 0s !important } }";
});
out.selection = await page.evaluate(() => getComputedStyle(document.getElementById("c"), "::selection").backgroundColor);
await page.evaluate(() => { document.getElementById("probe").textContent = ""; });
out.selectionWithout = await page.evaluate(() => getComputedStyle(document.getElementById("c"), "::selection").backgroundColor);
console.log(JSON.stringify(out, null, 1));
await browser.close();
