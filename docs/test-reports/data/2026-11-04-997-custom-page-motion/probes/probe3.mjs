// Probe of docs/test-reports/2026-11-04-997-custom-page-motion.md: named vs anonymous first layers, custom-property motion
// through inline !important, and the timing a comparison reads for edge-case Web Animations.
import { chromium } from "playwright-core";

const DECLS = "transition-property: none !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-play-state: running !important; animation-timeline: auto !important;";

const browser = await chromium.launch({ args: ["--site-per-process"] });
const page = await browser.newPage();
const out = {};
await page.setContent(`<!doctype html><html><head>
<style>@layer tq-motion { * { ${DECLS} } }</style>
<style>@layer { * { ${DECLS} } }</style>
</head><body>
<style>
  @keyframes k { from { opacity: 0 } to { opacity: 1 } }
  @layer tq-motion { #join { animation: k 5s !important; } }
  @layer { #anon { animation: k 5s !important; } }
  @keyframes swap { from { --wrong: " 999.99" } to { --wrong: " 999.99" } }
  #v::after { content: var(--wrong, ""); }
  .w:hover { --wrong: " 999.99"; }
</style>
<div id="join">j</div><div id="anon">a</div>
<div class="w" id="w1" style="animation: swap 1s 0.3s 1 !important"><b id="v">1</b></div>
<div class="w" id="w2" style="transition: --wrong 0s 0.5s allow-discrete !important"><b id="v2">2</b></div>
<style>#v2::after { content: var(--wrong, ""); }</style>
<div id="scroller" style="overflow:auto;height:50px"><div style="height:500px"></div></div>
<div id="t">t</div>
</body></html>`);
out.join = await page.evaluate(() => getComputedStyle(document.getElementById("join")).animationDuration);
out.anon = await page.evaluate(() => getComputedStyle(document.getElementById("anon")).animationDuration);
// The custom property animated from an inline !important animation.
out.customAnim = await page.evaluate(
  () =>
    new Promise((resolve) => {
      const seen = [];
      const look = () => {
        seen.push(getComputedStyle(document.getElementById("v"), "::after").content);
        if (seen.length < 120) requestAnimationFrame(look);
        else resolve({ distinct: [...new Set(seen)], live: document.getAnimations().map((a) => a.playState) });
      };
      look();
    }),
);
const w2 = await page.locator("#w2").boundingBox();
await page.mouse.move(w2.x + 2, w2.y + 2);
out.customTransition = await page.evaluate(
  () =>
    new Promise((resolve) => {
      const first = document.getAnimations().map((a) => [a.constructor.name, a.transitionProperty, a.playState, a.pending]);
      setTimeout(() => resolve({ first, after: getComputedStyle(document.getElementById("v2"), "::after").content }), 900);
    }),
);
out.timing = await page.evaluate(() => {
  const el = document.getElementById("t");
  const read = (a) => ({ end: a.effect.getComputedTiming().endTime, endType: typeof a.effect.getComputedTiming().endTime, t: a.currentTime, tType: typeof a.currentTime, rate: a.playbackRate, state: a.playState, pending: a.pending, fill: a.effect.getComputedTiming().fill });
  const r = {};
  const scroll = el.animate([{ opacity: 0 }, { opacity: 1 }], { timeline: new ScrollTimeline({ source: document.getElementById("scroller") }), duration: 0, delay: 0, endDelay: 0 });
  r.scroll = read(scroll);
  const a = el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 0, fill: "forwards" });
  r.fresh = read(a);
  a.updatePlaybackRate(-1);
  r.updateWhilePending = read(a);
  const rateSet = Object.getOwnPropertyDescriptor(Animation.prototype, "playbackRate").set;
  rateSet.call(a, -1);
  r.afterSetter = read(a);
  const b = el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 0, fill: "both" });
  b.reverse();
  r.reverseWhilePending = read(b);
  const c = el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 1000, delay: 500 });
  c.effect.updateTiming({ duration: 0, delay: 0, endDelay: 0 });
  r.updatedToZero = read(c);
  const d = new Animation(new KeyframeEffect(el, [{ opacity: 0 }, { opacity: 1 }], { duration: 0, fill: "forwards" }));
  d.play();
  d.currentTime = -2000;
  r.heldBack = read(d);
  const e = el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 0, iterations: Infinity, endDelay: 0 });
  r.infinite = read(e);
  const f = new Animation(new KeyframeEffect(el, [{ opacity: 0 }, { opacity: 1 }], { duration: 0 }), null);
  f.play();
  r.noTimeline = read(f);
  return r;
});
out.timingLater = await page.evaluate(() => document.getAnimations().map((a) => ({ state: a.playState, pending: a.pending, t: a.currentTime, rate: a.playbackRate })));
out.triggerNone = await page.evaluate(() => CSS.supports("animation-trigger", "none"));
console.log(JSON.stringify(out, null, 1));
await browser.close();
