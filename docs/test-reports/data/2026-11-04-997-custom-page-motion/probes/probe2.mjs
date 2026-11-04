// Probe of docs/test-reports/2026-11-04-997-custom-page-motion.md: which pseudo-elements Chromium animates or transitions, and
// whether a rule per pseudo-element in a first layer stops them.
import { chromium } from "playwright-core";

const PSEUDOS = [
  "::before", "::after", "::marker", "::backdrop", "::first-letter", "::first-line", "::placeholder", "::selection",
  "::file-selector-button", "::details-content", "::scroll-marker", "::scroll-marker-group", "::scroll-button(*)",
  "::column", "::picker(select)", "::picker-icon", "::checkmark", "::target-text", "::search-text",
  "::spelling-error", "::grammar-error", "::before::marker", "::after::marker", "::column::scroll-marker",
  "::view-transition", "::view-transition-group(*)", "::view-transition-old(*)", "::view-transition-new(*)", "::view-transition-image-pair(*)",
];
const DECLS = "transition-property: none !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-play-state: running !important; animation-timeline: auto !important; animation-trigger: none !important;";
const layer = (pseudos) => `@layer {\n* { ${DECLS} }\n${pseudos.map((p) => `${p} { ${DECLS} }`).join("\n")}\n}`;

const MOTION_ON_PSEUDOS = PSEUDOS.map((p) => `.on ${p.startsWith("::") ? "*" : ""}${p} { animation: fade 3s infinite; transition: opacity 2s, color 2s; }`).join("\n");

const PAGE = `<!doctype html><html><head><style id="runtime"></style></head><body>
<style>
  @keyframes fade { from { opacity: 0.2; color: red } to { opacity: 1; color: blue } }
  ${MOTION_ON_PSEUDOS}
  .t ${"*"}::before { opacity: 0.5 }
  li::before { content: "b" } li::after { content: "a" }
  li.mark::before { display: list-item; }
  .sc { overflow: auto; height: 60px; scroll-marker-group: after; }
  .sc > div { height: 50px; }
  .sc > div::scroll-marker { content: "m" }
  .cols { columns: 2; height: 40px; overflow: auto; scroll-marker-group: after; }
  .cols::column::scroll-marker { content: "c" }
  select, ::picker(select) { appearance: base-select; }
  li::before::marker { content: "x" }
</style>
<div class="on">
  <ul><li class="mark">first letter text</li></ul>
  <p>line</p>
  <input placeholder="ph"><input type="file">
  <details open><summary>s</summary>d</details>
  <div class="sc"><div>1</div><div>2</div></div>
  <div class="cols">a b c d e f g h i j k l m n o p</div>
  <select><option>o1</option><option>o2</option></select>
  <dialog id="dlg">dialog</dialog>
</div>
</body></html>`;

const browser = await chromium.launch({ args: ["--site-per-process"] });
const page = await browser.newPage();
await page.setContent(PAGE);
await page.evaluate(() => document.getElementById("dlg").showModal());
await page.locator("select").click().catch(() => {});
const live = () =>
  page.evaluate(() => {
    const out = {};
    for (const a of document.getAnimations()) {
      if (!(a.playState === "running" || a.playState === "paused" || a.pending)) continue;
      const key = `${a.constructor.name} ${a.effect?.pseudoElement ?? "(element)"}`;
      out[key] = (out[key] ?? 0) + 1;
    }
    return out;
  });
const result = { version: browser.version() };
result.supports = await page.evaluate((list) => Object.fromEntries(list.map((s) => [s, CSS.supports(`selector(${s})`)])), PSEUDOS);
result.triggerNone = await page.evaluate(() => CSS.supports("animation-trigger", "none"));
await page.waitForTimeout(200);
result.withoutLayer = await live();
await page.evaluate((css) => { document.getElementById("runtime").textContent = css; }, layer(PSEUDOS));
await page.waitForTimeout(200);
result.withLayerAnimations = await live();
// Transitions: change the animated properties' values now that the layer is on.
await page.evaluate(() => {
  const s = document.createElement("style");
  s.textContent = ".on *, .on *::before, .on *::after, .on *::marker, .on *::backdrop, .on *::placeholder { opacity: 0.4 !important; color: green !important }";
  document.body.append(s);
});
await page.waitForTimeout(50);
result.withLayerAfterChange = await live();
// The same without the layer, for transitions.
await page.evaluate(() => { document.getElementById("runtime").textContent = ""; });
await page.waitForTimeout(100);
await page.evaluate(() => {
  const s = document.createElement("style");
  s.textContent = ".on *, .on *::before, .on *::after, .on *::marker, .on *::backdrop, .on *::placeholder { opacity: 0.8 !important; color: purple !important }";
  document.body.append(s);
});
await page.waitForTimeout(50);
result.withoutLayerAfterChange = await live();
console.log(JSON.stringify(result, null, 1));
await browser.close();
