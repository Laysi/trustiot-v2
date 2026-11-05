// Probe of docs/test-reports/2026-11-05-1081-sampling-cpu.md: how much of the sandbox frame's
// main thread the display-time comparisons take while a page sits still and clean.
//
// The outer page is the custom-page browser test's harness (frontend/e2e/custom-page/harness.tsx:
// the real component, guard and host adapter); the sandbox files come from frontend/dist with the
// headers of `contract::api::dashboard::sandbox` (SANDBOX_DOCUMENT_HEADERS, SANDBOX_ASSET_HEADERS).
// Everything is served by Playwright routes: no server. Each case is a page of `marks` text marks
// (the fixture's first power cell) and `boxes` plain boxes that never changes after its first
// render, so after the frame is shown the only work in it is the runtime's: heartbeats and
// comparisons at random times (`sample`, two around a paint while the host may show the frame).
//
// For each case: wait until the outer page shows the frame, let it settle, then read the frame
// process's main-thread metrics (CDP `Performance.getMetrics`, `threadTicks`: CPU time) at the
// start and end of a window, and count the `compared` reports the host got in the window (a clean
// comparison reports one while the host may show the frame). Output: one JSON object per case.
//
// Usage (in the pinned Playwright container, see run.sh):
//   node probe.mjs '<cases JSON>'   e.g. '[{"name":"m1000","marks":1000,"boxes":4000,"seconds":60}]'
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { chromium } from "playwright-core";

const DIST = process.env.DIST ?? "/probe/dist";
const HARNESS = process.env.HARNESS ?? "/probe/harness/harness.js";
const ORIGIN = "http://localhost:4173";
const ROOT = "/__e2e/custom-page/";
const TITLE = "E2E custom page";
const SETTLE_MS = 5_000;
const SHOWN_WITHIN_MS = 120_000;

const SANDBOX_CSP =
  "sandbox allow-scripts; default-src 'none'; script-src 'self' 'unsafe-eval'; " +
  "style-src 'self' 'unsafe-inline'; img-src 'none'; font-src 'none'; connect-src 'none'; " +
  "media-src 'none'; frame-src 'none'; child-src 'none'; worker-src 'none'; manifest-src 'none'; " +
  "object-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'";
const DOCUMENT_HEADERS = {
  "content-security-policy": SANDBOX_CSP,
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "cross-origin-resource-policy": "cross-origin",
};
const ASSET_HEADERS = { "x-content-type-options": "nosniff", "cross-origin-resource-policy": "cross-origin" };
const TYPES = { html: "text/html; charset=utf-8", js: "text/javascript; charset=utf-8", css: "text/css; charset=utf-8" };

const WRONG = "999.99";
/**
 * The unnotified attack of the 2026-11-03 probes (f-nocover): an SVG `<style>` that shows WRONG for
 * the first value, turned on in every animation frame callback and off in the first task after the
 * paint, without cover. Its `disabled` setter is wrapped by seal.ts; the attack runs only against a
 * build with that wrap taken out (run.sh's caller does it), as a stand-in for a path the runtime is
 * not told of.
 */
const ATTACK = `<svg width="0" height="0"><style id="fake">#v { font-size: 0 } #v::after { content: "${WRONG}"; font-size: 1rem }</style></svg>
<p>Power <b id="v"></b> kW</p>`;
const ATTACK_SCRIPT = `
  const fake = document.getElementById("fake");
  fake.disabled = true;
  const channel = new MessageChannel();
  const off = () => { fake.disabled = true; };
  channel.port1.onmessage = () => { off(); requestAnimationFrame(on); };
  const on = () => { fake.disabled = false; channel.port2.postMessage(0); };
  window.startAttack = () => requestAnimationFrame(on);`;

/**
 * A page that never changes after its first render: `marks` text marks and `boxes` boxes; with
 * `attack`, also the first power value and the attack, which starts when the probe calls it.
 */
function staticPage(marks, boxes, attack = false) {
  return `${attack ? ATTACK : ""}<style>#m span{display:inline-block;width:52px;font:10px sans-serif}#b i{display:inline-block;width:3px;height:3px;margin:0 1px 1px 0;background:#888}</style>
<div id="m"></div><div id="b"></div>
<script>
  const m = document.getElementById("m");
  const b = document.getElementById("b");
  for (let i = 0; i < ${marks}; i++) m.appendChild(document.createElement("span"));
  for (let i = 0; i < ${boxes}; i++) b.appendChild(document.createElement("i"));
  tq.on("data", (tq) => {
    for (const span of m.children) tq.text(span, "c|power|kw|0", ",.2f");
    ${attack ? 'tq.text(document.getElementById("v"), "c|power|kw|0", ",.2f");' : ""}
  });${attack ? ATTACK_SCRIPT : ""}
</script>`;
}

async function until(what, condition, limitMs) {
  const deadline = Date.now() + limitMs;
  while (!(await condition())) {
    if (Date.now() > deadline) throw new Error(`no ${what} within ${limitMs} ms`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}

function metricsOf(result) {
  const out = {};
  for (const { name, value } of result.metrics) out[name] = value;
  return out;
}

async function runCase(browser, test) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
  const harness = readFileSync(HARNESS, "utf8");
  await context.route("**/*", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === ROOT) {
      await route.fulfill({
        status: 200,
        headers: { "content-type": TYPES.html },
        body: `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>probe</title><script type="module" src="${ROOT}harness.js"></script></head><body><div id="root"></div></body></html>`,
      });
    } else if (path === `${ROOT}harness.js`) {
      await route.fulfill({ status: 200, headers: { "content-type": TYPES.js }, body: harness });
    } else if (path === `${ROOT}case.json`) {
      await route.fulfill({
        status: 200,
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ page: staticPage(test.marks, test.boxes, test.attack === true) }),
      });
    } else if (/^\/sandbox\/v1\/(sandbox\.html|runtime\.js|d3\.js|sandbox\.css)$/.test(path)) {
      const ext = path.split(".").at(-1);
      const headers = ext === "html" ? DOCUMENT_HEADERS : ASSET_HEADERS;
      await route.fulfill({
        status: 200,
        headers: { ...headers, "content-type": TYPES[ext] },
        body: readFileSync(join(DIST, path)),
      });
    } else {
      await route.fulfill({ status: 404, body: "" });
    }
  });
  const page = await context.newPage();
  await page.goto(ORIGIN + ROOT);
  const recorded = () => page.evaluate(() => window.__customPage);
  const shownAt = Date.now();
  await until("frame shown", async () => (await recorded()).visibility.at(-1)?.visible === true, SHOWN_WITHIN_MS);
  const shownAfterMs = Date.now() - shownAt;
  await page.waitForTimeout(SETTLE_MS);
  const handle = await page.locator(`iframe[title="${TITLE}"]`).elementHandle();
  const frame = await handle.contentFrame();
  const elements = await frame.evaluate(() => document.getElementsByTagName("*").length);
  if (test.attack === true) {
    const result = await attack(page, frame, recorded, test);
    await context.close();
    return { name: test.name, marks: test.marks, boxes: test.boxes, elements, shownAfterMs, ...result };
  }
  const cdp = await context.newCDPSession(frame);
  await cdp.send("Performance.enable", { timeDomain: "threadTicks" });
  const before = metricsOf(await cdp.send("Performance.getMetrics"));
  const startReports = (await recorded()).reports.length;
  const startVisibility = (await recorded()).visibility.length;
  const wallStart = Date.now();
  await page.waitForTimeout(test.seconds * 1000);
  const after = metricsOf(await cdp.send("Performance.getMetrics"));
  const wallMs = Date.now() - wallStart;
  const record = await recorded();
  const window = record.reports.slice(startReports);
  const count = (type) => window.filter((report) => report.message.type === type).length;
  const taskMs = (after.TaskDuration - before.TaskDuration) * 1000;
  const compared = count("compared");
  const result = {
    name: test.name,
    marks: test.marks,
    boxes: test.boxes,
    elements,
    shownAfterMs,
    wallMs,
    taskMs: Math.round(taskMs),
    scriptMs: Math.round((after.ScriptDuration - before.ScriptDuration) * 1000),
    layoutMs: Math.round((after.LayoutDuration - before.LayoutDuration) * 1000),
    styleMs: Math.round((after.RecalcStyleDuration - before.RecalcStyleDuration) * 1000),
    cpuShare: Number((taskMs / wallMs).toFixed(4)),
    compared,
    beats: count("beat"),
    failed: window.filter((report) => report.message.type === "failed").map((report) => report.message.reason),
    findings: window.filter((report) => report.message.type === "compared" && report.message.findings > 0).length,
    visibilityChanges: record.visibility.length - startVisibility,
    shownAtEnd: record.visibility.at(-1)?.visible === true,
    // Every compared report is one comparison; the beats cost next to nothing.
    msPerComparison: compared > 0 ? Number((taskMs / compared).toFixed(1)) : null,
    meanMsBetweenComparisons: compared > 0 ? Math.round(wallMs / compared) : null,
  };
  await context.close();
  return result;
}

/**
 * Starts the attack and waits until the runtime closes the page or `seconds` pass. Returns how long
 * the outer page showed the frame between the start and the close (or the end), from its own
 * visibility record (the frame and the outer page share the clock).
 */
async function attack(page, frame, recorded, test) {
  const startReports = (await recorded()).reports.length;
  const startedAt = Date.now();
  await frame.evaluate(() => window.startAttack());
  const deadline = startedAt + test.seconds * 1000;
  let closed = null;
  while (Date.now() < deadline) {
    const record = await recorded();
    const failed = record.reports.slice(startReports).find((report) => report.message.type === "failed");
    if (failed !== undefined) {
      closed = failed;
      break;
    }
    await page.waitForTimeout(100);
  }
  const record = await recorded();
  const end = closed === null ? Date.now() : closed.at;
  let shown = 0;
  let visible = false;
  let since = startedAt;
  for (const entry of record.visibility) {
    if (entry.at <= startedAt) {
      visible = entry.visible;
      continue;
    }
    if (entry.at >= end) break;
    if (visible) shown += entry.at - since;
    visible = entry.visible;
    since = entry.at;
  }
  if (visible) shown += end - Math.max(since, startedAt);
  const window = record.reports.slice(startReports).filter((report) => report.at <= end);
  return {
    attack: "every-frame",
    closed: closed === null ? "NOT-CLOSED" : `failed/${closed.message.reason}`,
    closedAfterMs: closed === null ? null : closed.at - startedAt,
    shownDuringAttackMs: shown,
    comparedDuringAttack: window.filter((report) => report.message.type === "compared").length,
  };
}

const cases = JSON.parse(process.argv[2] ?? "[]");
const browser = await chromium.launch({ args: ["--site-per-process"] });
process.stdout.write(`${JSON.stringify({ chromium: browser.version(), arch: process.arch })}\n`);
for (const test of cases) {
  try {
    process.stdout.write(`${JSON.stringify(await runCase(browser, test))}\n`);
  } catch (error) {
    process.stdout.write(`${JSON.stringify({ name: test.name, error: String(error) })}\n`);
  }
}
await browser.close();
