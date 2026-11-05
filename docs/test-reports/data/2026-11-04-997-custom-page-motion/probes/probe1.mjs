// Probe of docs/test-reports/2026-11-04-997-custom-page-motion.md: how Chromium cascades a first anonymous layer's !important
// motion rules, and whether 0 s animations are pending or running when a comparison would run.
import { chromium } from "playwright-core";

const MOTION = `@layer {
  * { transition-property: none !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-play-state: running !important; animation-timeline: auto !important; }
  ::before { transition-property: none !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-play-state: running !important; animation-timeline: auto !important; }
  ::after { transition-property: none !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-play-state: running !important; animation-timeline: auto !important; }
  ::marker { transition-property: none !important; animation-duration: 0s !important; animation-delay: 0s !important; animation-play-state: running !important; animation-timeline: auto !important; }
}`;

const PAGE = `<!doctype html><html><head><style id="runtime">${MOTION}</style></head><body>
<style>
  @keyframes grow { from { transform: scale(1) } to { transform: scale(2) } }
  @keyframes fade { from { opacity: 0.2 } to { opacity: 1 } }
  #unlayered { animation: fade 2s 1s infinite paused !important; transition: opacity 1s !important; }
  @layer evil { #layered { animation: fade 2s 1s infinite paused !important; transition: opacity 1s !important; } }
  @layer evil2 { #layered2 { animation-duration: 5s !important; } }
  #inline { }
  #hov { width: 50px; height: 50px; background: red; transition: background-color 0.3s 0.2s; }
  #hov:hover { background: blue; animation: grow 300ms forwards; }
  #pse::after { content: "x"; animation: fade 2s infinite !important; transition: opacity 1s !important; }
  #pse::before { content: "y"; animation: fade 2s infinite; }
  li::marker { animation: fade 2s infinite; }
  #scroll { animation: fade linear; animation-timeline: scroll(); }
  #neg { animation: fade 2s -5s infinite; }
</style>
<div id="unlayered">u</div><div id="layered">l</div><div id="layered2">l2</div>
<div id="inline" style="animation: fade 2s 1s infinite !important; transition: opacity 1s 0.5s !important">i</div>
<div id="hov">h</div><div id="pse">p</div><ul><li>m</li></ul><div id="scroll">s</div><div id="neg">n</div>
<div id="waapi">w</div>
</body></html>`;

const browser = await chromium.launch({ args: ["--site-per-process"] });
const page = await browser.newPage();
await page.setContent(PAGE);
const out = {};
out.version = browser.version();
out.cascade = await page.evaluate(() => {
  const read = (id, pseudo) => {
    const s = getComputedStyle(document.getElementById(id), pseudo ?? null);
    return {
      duration: s.animationDuration,
      delay: s.animationDelay,
      play: s.animationPlayState,
      timeline: s.animationTimeline,
      transition: s.transitionProperty,
      transitionDuration: s.transitionDuration,
    };
  };
  return {
    unlayered: read("unlayered"),
    layered: read("layered"),
    layered2: read("layered2"),
    inline: read("inline"),
    pseAfter: read("pse", "::after"),
    pseBefore: read("pse", "::before"),
    marker: getComputedStyle(document.querySelector("li"), "::marker").animationDuration,
    scroll: read("scroll"),
    neg: read("neg"),
  };
});
const animations = () =>
  page.evaluate(() =>
    document.getAnimations().map((a) => ({
      name: a.animationName ?? a.transitionProperty ?? a.id ?? a.constructor.name,
      target: a.effect?.target?.id ?? a.effect?.target?.localName,
      pseudo: a.effect?.pseudoElement ?? null,
      playState: a.playState,
      pending: a.pending,
      endTime: a.effect?.getComputedTiming().endTime,
      currentTime: a.currentTime,
      rate: a.playbackRate,
      fill: a.effect?.getComputedTiming().fill,
    })),
  );
out.atLoad = await animations();
// A hover: record the animations when the runtime would compare (in the next animation frame, from a
// pointerover listener), and once more after that frame.
await page.evaluate(() => {
  window.seen = [];
  const snap = (when) =>
    window.seen.push({
      when,
      list: document
        .getAnimations()
        .filter((a) => a.effect?.target?.id === "hov")
        .map((a) => ({ name: a.animationName ?? a.transitionProperty, playState: a.playState, pending: a.pending, endTime: a.effect.getComputedTiming().endTime, currentTime: a.currentTime })),
      bg: getComputedStyle(document.getElementById("hov")).backgroundColor,
      transform: getComputedStyle(document.getElementById("hov")).transform,
    });
  addEventListener(
    "pointerover",
    (e) => {
      if (e.target.id !== "hov") return;
      snap("listener");
      queueMicrotask(() => snap("microtask"));
      requestAnimationFrame(() => {
        snap("rAF");
        requestAnimationFrame(() => snap("rAF2"));
      });
      setTimeout(() => snap("timeout0"), 0);
    },
    true,
  );
});
const box = await page.locator("#hov").boundingBox();
await page.mouse.move(box.x + 10, box.y + 10);
await page.waitForTimeout(300);
out.hover = await page.evaluate(() => window.seen);
// Clamped animate(): what a comparison right after it (end of the task) and in the next frame sees.
out.waapi = await page.evaluate(
  () =>
    new Promise((resolve) => {
      const el = document.getElementById("waapi");
      const snaps = [];
      const snap = (when, a) =>
        snaps.push({ when, playState: a.playState, pending: a.pending, endTime: a.effect.getComputedTiming().endTime, currentTime: a.currentTime, transform: getComputedStyle(el).transform });
      const a = el.animate([{ transform: "translateX(0)" }, { transform: "translateX(40px)" }], { duration: 0, delay: 0, endDelay: 0, fill: "forwards", iterations: Infinity });
      snap("sync", a);
      queueMicrotask(() => snap("microtask", a));
      requestAnimationFrame(() => {
        snap("rAF", a);
        requestAnimationFrame(() => {
          snap("rAF2", a);
          resolve(snaps);
        });
      });
    }),
);
// A page sheet put ahead of the runtime's: its layer is declared first.
out.prepended = await page.evaluate(() => {
  const s = document.createElement("style");
  s.textContent = "@layer first { #layered2 { animation-duration: 7s !important } }";
  document.head.prepend(s);
  return getComputedStyle(document.getElementById("layered2")).animationDuration;
});
out.supports = await page.evaluate(() =>
  Object.fromEntries(
    [
      "::before", "::after", "::marker", "::backdrop", "::first-letter", "::first-line", "::placeholder", "::selection",
      "::file-selector-button", "::details-content", "::scroll-marker", "::scroll-marker-group", "::scroll-button(*)",
      "::column", "::picker(select)", "::picker-icon", "::checkmark", "::target-text", "::view-transition", "::search-text",
      "::cue", "::part(x)", "::slotted(*)", "::highlight(x)", "::spelling-error", "::grammar-error", "::interest-hint",
    ].map((s) => [s, CSS.supports(`selector(${s})`)]),
  ),
);
out.props = await page.evaluate(() =>
  Object.fromEntries(
    ["animation-trigger", "timeline-trigger", "animation-range", "transition-behavior", "animation-composition", "interpolate-size"].map((p) => [p, CSS.supports(p, "initial")]),
  ),
);
console.log(JSON.stringify(out, null, 1));
await browser.close();
