// The dashboard's custom page in a real Chromium (Laysi/trustiot-v2#997; frontend/sandbox/README.md,
// 「顯示時比對的強制」): the outer page is the real component, guard and host adapter
// (custom-page/harness.tsx, bundled by custom-page/build.ts), served here under the SPA's real
// headers copied from `services/web`; the sandbox document and its scripts come from the real
// `services/web` with their real headers. Every page below is an adversary: it tampers with its
// document after the comparison, forges or floods messages, blocks its thread, navigates, or tries
// to reach the port. The outer page must show the frame only on a clean result received on the
// port, and hide or remove it on any finding, any failure signal or a missed heartbeat.
//
// Chromium runs with `--site-per-process`, as desktop Chrome does by default: the sandboxed frame
// (an opaque origin) then has a process of its own, so a page that blocks its thread cannot block
// the outer page (README, 「檢查不到的部分」, for a browser without it).
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { expect, type Frame, type Page, test } from "@playwright/test";

test.use({ launchOptions: { args: ["--site-per-process"] } });

const HARNESS = join(import.meta.dirname, "custom-page", "dist", "harness.js");
const ROOT = "/__e2e/custom-page/";
const TITLE = "E2E custom page";

/** The heartbeat's timeout (frontend/sandbox/src/protocol.ts, HEARTBEAT_TIMEOUT_MS). */
const HEARTBEAT_TIMEOUT_MS = 10_000;
/** How long the outer page waits for any report before it hides the frame (protocol.ts, STALE_AFTER_MS). */
const STALE_AFTER_MS = 1_000;
/** How often a page may lose its shown state before the frame is closed (guard.ts, MAX_REGRESSIONS). */
const MAX_REGRESSIONS = 3;
/** A generous bound for what the outer page waits for, far above the heartbeat timeout. */
const WAIT_MS = 60_000;

/** The value the fixture's first power cell shows with `,.2f` (fixtures.ts, POWER_ROWS). */
const RIGHT = "12.35";
const WRONG = "999.99";
const MARKUP = `<img src="/x" onerror="parent.postMessage('owned','*')"><script>window.owned=1</script>`;

interface PortReport {
  at: number;
  message: { type: string; generation?: number; findings?: number; reason?: string };
  visibleAfter: boolean;
}

interface Recorded {
  reports: PortReport[];
  visibility: { at: number; visible: boolean }[];
  navigations: number[];
  ready: boolean;
}

/** A page that draws the first power cell into `#v` and the line name into `#l`. */
function drawing(extra = ""): string {
  return `<p>Power <b id="v"></b> kW on <b id="l"></b></p>
<script>
  window.renders = 0;
  tq.on("data", (tq) => {
    tq.text(document.getElementById("v"), "c|power|kw|0", ",.2f");
    tq.text(document.getElementById("l"), "c|power|line|0");
    window.renders++;
  });
  ${extra}
</script>`;
}

/**
 * Serves the harness under the SPA's real headers (from a real response of `services/web`) and
 * opens it with `testCase`. Returns the page's record and the frame.
 */
async function open(page: Page, testCase: Record<string, unknown>) {
  const spa = await page.request.get("/");
  expect(spa.status(), "the SPA's index").toBe(200);
  const headers: Record<string, string> = {};
  for (const [name, value] of Object.entries(spa.headers())) {
    if (["content-length", "content-encoding", "etag", "date", "transfer-encoding", "connection"].includes(name))
      continue;
    headers[name] = value;
  }
  const harness = readFileSync(HARNESS, "utf8");
  await page.route(`**${ROOT}**`, async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === ROOT) {
      await route.fulfill({
        status: 200,
        headers: { ...headers, "content-type": "text/html; charset=utf-8" },
        body: `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>custom page</title><script type="module" src="${ROOT}harness.js"></script></head><body><div id="root"></div></body></html>`,
      });
    } else if (path === `${ROOT}harness.js`) {
      await route.fulfill({
        status: 200,
        headers: { ...headers, "content-type": "text/javascript; charset=utf-8" },
        body: harness,
      });
    } else if (path === `${ROOT}case.json`) {
      await route.fulfill({
        status: 200,
        headers: { ...headers, "content-type": "application/json" },
        body: JSON.stringify(testCase),
      });
    } else {
      await route.fulfill({ status: 404, body: "" });
    }
  });
  await page.goto(ROOT);
  const recorded = () => page.evaluate(() => (window as unknown as { __customPage: Recorded }).__customPage);
  const frame = async (): Promise<Frame> => {
    const handle = await page.locator(`iframe[title="${TITLE}"]`).elementHandle({ timeout: WAIT_MS });
    const content = await handle?.contentFrame();
    if (content === null || content === undefined) throw new Error("the custom page's frame has no document");
    return content;
  };
  return { recorded, frame, spaCsp: headers["content-security-policy"] ?? "" };
}

async function waitVisible(recorded: () => Promise<Recorded>, what: string): Promise<void> {
  await expect
    .poll(async () => (await recorded()).visibility.at(-1)?.visible, {
      message: `the frame shown: ${what}`,
      timeout: WAIT_MS,
    })
    .toBe(true);
}

/** Waits until the page's own `window[name]` is set, and returns it. */
async function pageValue(frame: Frame, name: string, what: string): Promise<unknown> {
  await expect
    .poll(() => frame.evaluate((key) => (window as unknown as Record<string, unknown>)[key] !== undefined, name), {
      message: what,
      timeout: WAIT_MS,
    })
    .toBe(true);
  return frame.evaluate((key) => (window as unknown as Record<string, unknown>)[key], name);
}

/** Whether the frame was visible at any recorded moment at or after `from`. */
function shownSince(record: Recorded, from: number): boolean {
  let visible = false;
  for (const entry of record.visibility) {
    if (entry.at < from) {
      visible = entry.visible;
      continue;
    }
    if (entry.visible) return true;
    visible = entry.visible;
  }
  // Still visible from before `from`, and never hidden since.
  return visible && record.visibility.every((entry) => entry.at < from);
}

/** The first report with findings at or after `from`, and the report just before it. */
function firstFinding(record: Recorded, from: number): { finding: PortReport; before: PortReport | undefined } {
  const index = record.reports.findIndex(
    (report) => report.at >= from && report.message.type === "compared" && (report.message.findings ?? 0) > 0,
  );
  const finding = record.reports[index];
  if (finding === undefined) throw new Error("no comparison with findings was reported");
  return { finding, before: record.reports[index - 1] };
}

/**
 * The change at `at` was found by the comparison the runtime ran because it was told of the change:
 * a finding was reported after it, and nothing reported `failed`. A change the runtime is not told
 * of is found by a comparison at a random time instead, which closes the page with `failed`
 * (`unnotified`). (The first report after `at` may still be a clean one: a comparison that ran just
 * before the change and reached the outer page after it.)
 */
function expectComparedForTheChange(record: Recorded, at: number): void {
  const found = record.reports.some(
    (report) => report.at >= at && report.message.type === "compared" && (report.message.findings ?? 0) > 0,
  );
  expect(found, "a finding reported after the change").toBe(true);
  expect(record.reports.some((report) => report.message.type === "failed")).toBe(false);
}

/**
 * Pages that log what they paint: `watchPaint(test)` checks `test()` in every animation frame (just
 * before the frame's paint) and records each change of it in `window.paintLog` as
 * `[Date.now(), wrong, the previous frame's Date.now()]`. It reads the page's own state only; the in-frame hiding the runtime adds
 * (`data-tq-unverified`) is ignored, so what it measures is what the outer page alone keeps off the
 * screen.
 */
/** `[when, wrong, when the frame before was]`, from `WATCH_PAINT`. */
type PaintEntry = [number, boolean, number];

const WATCH_PAINT = `window.paintLog = [];
const watchPaint = (wrong) => {
  let last = null;
  let before = Date.now();
  const look = () => {
    const now = Boolean(wrong());
    const at = Date.now();
    if (now !== last) { last = now; window.paintLog.push([at, now, before]); }
    before = at;
    requestAnimationFrame(look);
  };
  requestAnimationFrame(look);
};`;

/**
 * Keeps reading the frame's `window.paintLog` until the frame is gone or `untilMs` passed, and
 * returns the last one read (the frame may be removed, and its log with it).
 */
async function followPaintLog(page: Page, untilMs: number): Promise<PaintEntry[]> {
  let last: PaintEntry[] = [];
  const end = Date.now() + untilMs;
  while (Date.now() < end) {
    const handle = await page
      .locator(`iframe[title="${TITLE}"]`)
      .elementHandle({ timeout: 1_000 })
      .catch(() => null);
    if (handle === null) break;
    const content = await handle.contentFrame();
    const read = await content
      ?.evaluate(() => (window as unknown as { paintLog?: PaintEntry[] }).paintLog)
      .catch(() => undefined);
    if (read !== undefined) last = read;
    await page.waitForTimeout(25);
  }
  return last;
}

/**
 * How long a wrong value was painted in the frame while the outer page showed the frame, in ms:
 * `total`, and `beyondReport`, the part after the port report of a finding (or a failure) for the
 * change that painted it. The change happened between the frame before the wrong paint and that
 * paint, so the report is the first one after the frame before. A change through a path the runtime
 * is told of is compared and reported at the end of its task, before the frame paints it: what may
 * remain before the report is the time the report takes to reach the outer page. `latencies` is the
 * time from the frame before each wrong paint to that report.
 */
function exposure(record: Recorded, paintLog: PaintEntry[], end: number) {
  // The frame's state after each report too: the harness reads it when the component has handled
  // the report, while its own observer of the frame records the change a moment later.
  const timeline = [...record.visibility, ...record.reports.map((r) => ({ at: r.at, visible: r.visibleAfter }))].sort(
    (a, b) => a.at - b.at,
  );
  const shownAt = (t: number) => {
    let shown = false;
    for (const entry of timeline) if (entry.at <= t) shown = entry.visible;
    return shown;
  };
  let total = 0;
  let beyondReport = 0;
  const latencies: number[] = [];
  for (let i = 0; i < paintLog.length; i++) {
    const [from, wrong, before] = paintLog[i] ?? [0, false, 0];
    if (!wrong) continue;
    const to = paintLog[i + 1]?.[0] ?? end;
    const report =
      record.reports.find((r) => r.at >= before && ((r.message.findings ?? 0) > 0 || r.message.type === "failed"))
        ?.at ?? Number.POSITIVE_INFINITY;
    if (Number.isFinite(report)) latencies.push(report - before);
    for (let t = from; t < to; t++) {
      if (!shownAt(t)) continue;
      total++;
      if (t >= report) beyondReport++;
    }
  }
  return { total, beyondReport, latencies };
}

/** Keeps a measurement in the test's result (the JSON report's annotations), for the test report. */
function note(what: string, value: unknown): void {
  test.info().annotations.push({ type: `exposure: ${what}`, description: JSON.stringify(value) });
}

async function noticeText(page: Page): Promise<string> {
  return (await page.getByTestId("custom-page-notice").textContent({ timeout: WAIT_MS })) ?? "";
}

test.describe("shown only on a clean result over the port", () => {
  test("a page whose values match the data is shown, from a sandboxed document, and stays shown past the heartbeat timeout", async ({
    page,
  }) => {
    const sandboxResponses: { url: string; headers: Record<string, string> }[] = [];
    page.on("response", (response) => {
      if (new URL(response.url()).pathname.startsWith("/sandbox/")) {
        sandboxResponses.push({ url: new URL(response.url()).pathname, headers: response.headers() });
      }
    });
    const { recorded, frame, spaCsp } = await open(page, { page: drawing() });
    const host = new URL(page.url()).host;
    // The SPA's CSP lets only the sandbox document on this host be framed.
    expect(spaCsp).toContain(`; frame-src ${host}/sandbox/v1/sandbox.html`);
    expect(spaCsp).toContain("frame-ancestors 'none'");
    await waitVisible(recorded, "a clean page");
    const content = await frame();
    expect(await content.evaluate(() => document.getElementById("v")?.textContent)).toBe(RIGHT);
    expect(await content.evaluate(() => origin)).toBe("null");
    const shownAt = Date.now();
    // Every report that showed it was a clean comparison on the port.
    const record = await recorded();
    const firstShown = record.reports.find((report) => report.visibleAfter);
    expect(firstShown?.message).toMatchObject({ type: "compared", findings: 0, generation: 1 });

    const documentResponse = sandboxResponses.find((response) => response.url === "/sandbox/v1/sandbox.html");
    expect(documentResponse?.headers["content-security-policy"]).toMatch(/^sandbox allow-scripts;/);
    expect(documentResponse?.headers["content-security-policy"]).toContain("frame-ancestors 'self'");
    expect(documentResponse?.headers["x-frame-options"]).toBeUndefined();
    for (const response of sandboxResponses) {
      expect(response.headers["cross-origin-resource-policy"], response.url).toBe("cross-origin");
    }

    // The heartbeat keeps it shown longer than the timeout (the test's own configured duration). (A
    // loaded machine may stall the frame's thread past STALE_AFTER_MS, which hides it for a moment:
    // this checks it is not closed, and shown at the end.)
    await page.waitForTimeout(HEARTBEAT_TIMEOUT_MS + 2_000);
    await waitVisible(recorded, "the clean page after the heartbeat timeout");
    const later = await recorded();
    expect(later.reports.some((report) => report.message.type === "failed")).toBe(false);
    expect(later.navigations).toEqual([]);
    await expect(page.locator(`iframe[title="${TITLE}"]`)).toHaveCount(1);
    expect(
      later.reports.filter((report) => report.at >= shownAt && report.message.type === "beat").length,
    ).toBeGreaterThan(5);
    // Comparisons run between beats too, at random times, while nothing changes.
    expect(
      later.reports.filter((report) => report.at >= shownAt && report.message.type === "compared").length,
    ).toBeGreaterThan(5);
  });

  test("a click on a marked value opens its source: the dataset's SQL and query time", async ({ page }) => {
    const { recorded, frame } = await open(page, { page: drawing() });
    await waitVisible(recorded, "a clean page");
    await (await frame()).locator("#v").click();
    // The source panel is a region next to the frame (SourcePanel.tsx), as for the components.
    const panel = page.getByRole("region", { name: "Source" });
    await expect(panel, "the source panel").toBeVisible({ timeout: WAIT_MS });
    await expect(panel).toContainText("SELECT ts, line, kw FROM power ORDER BY ts");
    await expect(panel).toContainText("Line power");
    // QUERIED_AT of the fixture, 2026-11-01T02:30:00Z, in the en locale and UTC.
    await expect(panel).toContainText("Nov 1, 2026");
    await expect(panel).toContainText("2:30:00");
  });

  test("a refresh hides it until the new data is compared, then shows it again", async ({ page }) => {
    const { recorded } = await open(page, { page: drawing(), refreshAfterMs: 3_000 });
    await waitVisible(recorded, "the first data");
    await expect
      .poll(
        async () => (await recorded()).reports.some((r) => r.message.type === "compared" && r.message.generation === 2),
        {
          message: "a comparison of the refreshed data",
          timeout: WAIT_MS,
        },
      )
      .toBe(true);
    await waitVisible(recorded, "the refreshed data");
    const record = await recorded();
    const second = record.reports.find((r) => r.message.type === "compared" && r.message.generation === 2);
    const firstShown = record.visibility.find((entry) => entry.visible);
    // Hidden between the refresh and the first comparison of its data.
    const hidden = record.visibility.find(
      (entry) => !entry.visible && entry.at >= (firstShown?.at ?? Infinity) && entry.at <= (second?.at ?? 0),
    );
    expect(hidden, "hidden while the new data was not yet compared").toBeDefined();
    expect(second?.visibleAfter).toBe(true);
  });
});

test.describe("what the page cannot reach", () => {
  test("no request leaves the frame, and it reads no cookie, storage or outer document", async ({
    page,
    context,
    baseURL,
  }) => {
    await context.addCookies([{ name: "e2e_secret", value: "fictional-session", url: baseURL ?? "" }]);
    // Every request from inside the frame and how it ended: CSP blocks a request after Chromium
    // reports it as started, so what matters is that none but the sandbox's own files completed.
    const fromFrame: { path: string; outcome: string }[] = [];
    const inFrame = (request: { frame(): Frame }) => request.frame() !== page.mainFrame();
    page.on("requestfinished", (request) => {
      if (inFrame(request)) fromFrame.push({ path: new URL(request.url()).pathname, outcome: "finished" });
    });
    page.on("requestfailed", (request) => {
      if (inFrame(request)) {
        fromFrame.push({ path: new URL(request.url()).pathname, outcome: request.failure()?.errorText ?? "failed" });
      }
    });
    const popups: string[] = [];
    context.on("page", (opened) => popups.push(opened.url()));
    const attempts = `
      const results = {};
      window.violations = [];
      document.addEventListener("securitypolicyviolation", (event) => {
        window.violations.push(event.effectiveDirective + " " + event.blockedURI);
      });
      const tryIt = async (name, run) => {
        try { const value = await run(); results[name] = "allowed:" + String(value).slice(0, 40); }
        catch (error) { results[name] = "blocked"; }
      };
      (async () => {
        await tryIt("fetch", () => fetch("/api/identity/v1/sessions/current").then((r) => r.status));
        await tryIt("xhr", () => new Promise((resolve, reject) => { const x = new XMLHttpRequest(); x.onload = () => resolve(x.status); x.onerror = () => reject(new Error("x")); x.open("GET", "/settings"); x.send(); }));
        await tryIt("image", () => new Promise((resolve, reject) => { const i = new Image(); i.onload = () => resolve("loaded"); i.onerror = () => reject(new Error("x")); i.src = "/favicon.svg"; }));
        await tryIt("websocket", () => new Promise((resolve, reject) => { const w = new WebSocket("ws://" + "localhost/"); w.onopen = () => resolve("open"); w.onerror = () => reject(new Error("x")); }));
        await tryIt("beacon", () => { if (!navigator.sendBeacon("/beacon", "x")) throw new Error("x"); return "queued"; });
        await tryIt("cookie", () => { document.cookie = "planted=1"; return document.cookie; });
        await tryIt("localStorage", () => localStorage.length);
        await tryIt("sessionStorage", () => sessionStorage.length);
        await tryIt("indexedDB", () => new Promise((resolve, reject) => { const r = indexedDB.open("x"); r.onsuccess = () => resolve("open"); r.onerror = () => reject(new Error("x")); }));
        await tryIt("parentDocument", () => parent.document.title);
        await tryIt("topLocation", () => top.location.href);
        // Chromium throws; a browser may also drop it silently: the outer page's address decides.
        await tryIt("topNavigate", () => { top.location.href = "https://example.com/"; return "attempted"; });
        // Loads that fail without an exception: CSP reports them, and no request may complete.
        const probe = document.createElement("i");
        probe.id = "probe";
        const sheet = document.createElement("style");
        sheet.textContent = "@font-face { font-family: probe; src: url(/probe-font.woff2); } #probe { font-family: probe; background-image: url(/probe-background.png); }";
        document.head.appendChild(sheet);
        document.body.appendChild(probe);
        probe.textContent = "probe";
        await tryIt("form", () => { const form = document.createElement("form"); form.action = "/probe-form"; form.method = "post"; document.body.appendChild(form); form.submit(); form.remove(); return "submitted"; });
        await new Promise((resolve) => setTimeout(resolve, 1000));
        probe.remove();
        sheet.remove();
        await tryIt("open", () => { const w = window.open("/settings"); if (w === null) throw new Error("x"); return "opened"; });
        await tryIt("worker", () => new Promise((resolve, reject) => { const w = new Worker("/static/x.js"); w.onerror = () => reject(new Error("x")); setTimeout(() => resolve("started"), 500); }));
        // A nested frame is content the comparison refuses, so the page removes it afterwards.
        await tryIt("frame", () => new Promise((resolve, reject) => { const f = document.createElement("iframe"); f.src = "/settings"; f.onload = () => { try { resolve(f.contentDocument ? "same-origin" : "loaded-opaque"); } catch { resolve("loaded-opaque"); } }; document.body.appendChild(f); setTimeout(() => reject(new Error("x")), 2000); }).finally(() => document.querySelector("iframe")?.remove()));
        window.results = results;
      })();`;
    const { recorded, frame } = await open(page, { page: drawing(attempts) });
    const content = await frame();
    const results = (await pageValue(content, "results", "the page's attempts")) as Record<string, string>;
    for (const name of [
      "fetch",
      "xhr",
      "image",
      "websocket",
      "localStorage",
      "sessionStorage",
      "indexedDB",
      "parentDocument",
      "topLocation",
      "open",
      "worker",
    ]) {
      expect(results[name], name).toBe("blocked");
    }
    // sendBeacon only queues: Chromium returns true and then CSP blocks the request itself, which
    // the frame reports as a connect-src violation (and no request is seen below).
    expect(["blocked", "allowed:queued"]).toContain(results.beacon);
    const violations = (await content.evaluate(() => (window as unknown as { violations: string[] }).violations)).join(
      "\n",
    );
    expect(violations, "the beacon's violation").toMatch(/^connect-src .*\/beacon$/m);
    expect(violations, "the font's violation").toMatch(/^font-src .*\/probe-font\.woff2$/m);
    expect(violations, "the background image's violation").toMatch(/^img-src .*\/probe-background\.png$/m);
    // A sandbox without allow-forms submits nothing (no request completes, checked below), and
    // without allow-top-navigation the page cannot navigate the outer page.
    expect(["blocked", "allowed:attempted"], "topNavigate").toContain(results.topNavigate);
    expect(page.url(), "the outer page's address").toContain("/__e2e/custom-page/");
    // The cookie API of an opaque origin throws, or at most sees nothing of the platform's.
    expect(
      results.cookie === "blocked" || !String(results.cookie).includes("e2e_secret"),
      `cookie: ${results.cookie}`,
    ).toBe(true);
    // A frame the page makes may not load anything (frame-src 'none'): never the SPA's document.
    expect(results.frame, "frame").not.toBe("allowed:same-origin");
    // Nothing but the sandbox's own files was fetched from inside the frame, and no window opened.
    const allowed = [
      "/sandbox/v1/sandbox.html",
      "/sandbox/v1/runtime.js",
      "/sandbox/v1/d3.js",
      "/sandbox/v1/sandbox.css",
      "/sandbox/v1/compare.js",
    ];
    const others = fromFrame.filter((request) => !allowed.includes(request.path));
    expect(
      others.filter((request) => request.outcome === "finished"),
      "requests from the frame that completed",
    ).toEqual([]);
    for (const request of others) {
      expect(request.outcome, `how ${request.path} ended`).toMatch(/^csp$|ERR_BLOCKED_BY_CSP|ERR_ABORTED/);
    }
    expect(popups, "windows the page opened").toEqual([]);
    // The attempts change no value: the page is still shown.
    await waitVisible(recorded, "a clean page that tried the network");
    expect(page.url(), "the outer page's address, later").toContain("/__e2e/custom-page/");
  });

  test("a frame the page creates is another origin, so it gives no unsealed realm", async ({ page }) => {
    // seal.ts wraps this realm's CSS object model and Web Animations API; a child frame's copy would
    // be unwrapped. The sandbox's CSP sandboxes every child into an origin of its own.
    const { frame } = await open(page, {
      page: drawing(`setTimeout(() => {
        const out = {};
        for (const kind of ["about:blank", "srcdoc", "no src"]) {
          const child = document.createElement("iframe");
          if (kind === "about:blank") child.src = "about:blank";
          if (kind === "srcdoc") child.srcdoc = "<p>child</p>";
          document.body.appendChild(child);
          try { out[kind] = "reached:" + typeof child.contentWindow.CSSStyleSheet.prototype.insertRule; }
          catch (error) { out[kind] = "blocked:" + error.name; }
          child.remove();
        }
        window.children = out;
      }, 1500);`),
    });
    expect(await pageValue(await frame(), "children", "the page's child frames")).toEqual({
      "about:blank": "blocked:SecurityError",
      srcdoc: "blocked:SecurityError",
      "no src": "blocked:SecurityError",
    });
  });

  test("values with markup in the data are shown as text", async ({ page }) => {
    const { recorded, frame } = await open(page, {
      page: drawing("setTimeout(() => { window.checked = document.querySelectorAll('img, b script').length; }, 1000);"),
      data: "markup",
    });
    await waitVisible(recorded, "a page with markup in a value");
    const content = await frame();
    expect(await content.evaluate(() => document.getElementById("l")?.textContent)).toBe(MARKUP);
    expect(await pageValue(content, "checked", "the page's count of elements")).toBe(0);
    expect(await content.evaluate(() => (window as unknown as { owned?: unknown }).owned)).toBeUndefined();
    expect(await content.evaluate(() => document.getElementById("l")?.childElementCount)).toBe(0);
  });

  test("the page finds no way to the port, and a page that would use one stays hidden", async ({ page }) => {
    const hunt = `
      window.found = [];
      const keep = (what, port) => { if (port) window.found.push(what); };
      // The event that carried the port has gone before any page script runs.
      keep("window.event", window.event && window.event.ports && window.event.ports[0]);
      // Every later message: the runtime's listener stops a port-carrying one before any other.
      addEventListener("message", (event) => keep("listener", event.ports && event.ports[0]), true);
      window.onmessage = (event) => keep("onmessage", event.ports && event.ports[0]);
      // A port the page makes itself (used below): not one the page found.
      const channel = new MessageChannel();
      const own = (port) => port === channel.port1 || port === channel.port2;
      // The runtime posts with the function it captured before the page ran.
      try {
        const original = MessagePort.prototype.postMessage;
        MessagePort.prototype.postMessage = function (...args) { if (!own(this)) keep("prototype", this); return original.apply(this, args); };
      } catch { window.found.push("prototype-locked"); }
      try {
        Object.defineProperty(MessageEvent.prototype, "ports", { get() { keep("getter", true); return []; } });
      } catch {}
      // A port the page makes itself goes nowhere the outer page listens.
      parent.postMessage({ tq: 1, type: "compared", generation: 1, findings: 0 }, "*", [channel.port2]);
      channel.port1.postMessage({ tq: 1, type: "compared", generation: 1, findings: 0 });
      // Draw a wrong value: only a clean report on the real port could show it.
      setTimeout(() => {
        document.getElementById("v").textContent = "${WRONG}";
        window.tamperedAt = Date.now();
      }, 1500);`;
    const { recorded, frame } = await open(page, { page: drawing(hunt) });
    const content = await frame();
    const tamperedAt = (await pageValue(content, "tamperedAt", "the page's wrong value")) as number;
    await expect
      .poll(async () => (await recorded()).reports.some((r) => r.at >= tamperedAt && (r.message.findings ?? 0) > 0), {
        message: "a comparison that found the wrong value",
        timeout: WAIT_MS,
      })
      .toBe(true);
    const found = (await content.evaluate(() => (window as unknown as { found: string[] }).found)).filter(
      (what) => what !== "prototype-locked",
    );
    expect(found, "ports the page got hold of").toEqual([]);
    // Wait two heartbeats more: it stays hidden, whatever the page posted.
    await page.waitForTimeout(2_500);
    expect(shownSince(await recorded(), tamperedAt)).toBe(false);
  });
});

test.describe("hidden on a finding", () => {
  test("a value the page changes after the comparison hides the frame in the report of that change, before any heartbeat", async ({
    page,
  }) => {
    const { recorded, frame } = await open(page, {
      page: drawing(
        `setTimeout(() => { document.getElementById("v").textContent = "${WRONG}"; window.tamperedAt = Date.now(); }, 2000);`,
      ),
    });
    await waitVisible(recorded, "the page before its change");
    const content = await frame();
    const tamperedAt = (await pageValue(content, "tamperedAt", "the page's change")) as number;
    await expect
      .poll(async () => (await recorded()).reports.some((r) => r.at >= tamperedAt && (r.message.findings ?? 0) > 0), {
        message: "the comparison of the change",
        timeout: WAIT_MS,
      })
      .toBe(true);
    const record = await recorded();
    const { finding } = firstFinding(record, tamperedAt);
    // Hidden by the component when it handled that report, not later.
    expect(finding.visibleAfter).toBe(false);
    // The change caused the comparison (README, 「顯示時比對的強制」).
    expectComparedForTheChange(record, tamperedAt);
    // In its own document the runtime replaced the value with 「—」 and flagged it (README, 「比對程式」).
    await expect
      .poll(() => content.evaluate(() => document.getElementById("v")?.textContent), {
        message: "the corrected value",
        timeout: WAIT_MS,
      })
      .toBe("—");
    expect(await content.evaluate(() => document.getElementById("v")?.hasAttribute("data-tq-flag"))).toBe(true);
    // The outer page does not show a corrected page either: it stays hidden and says why.
    await page.waitForTimeout(2_500);
    expect(shownSince(await recorded(), finding.at)).toBe(false);
    expect(await noticeText(page)).toMatch(/don't match the data/);
  });

  test("an unmatched value from the start: 「—」 and a flag in the frame, never shown", async ({ page }) => {
    const { recorded, frame } = await open(page, {
      page: `<p><b id="v" data-tq="c|power|kw|0">12.40</b></p><script>tq.on("data", () => { window.drawn = 1; });</script>`,
    });
    const content = await frame();
    await expect
      .poll(() => content.evaluate(() => document.getElementById("v")?.textContent), {
        message: "the corrected value",
        timeout: WAIT_MS,
      })
      .toBe("—");
    expect(await content.evaluate(() => document.getElementById("v")?.hasAttribute("data-tq-flag"))).toBe(true);
    await page.waitForTimeout(2_500);
    const record = await recorded();
    expect(record.visibility.some((entry) => entry.visible)).toBe(false);
    expect(record.reports.some((r) => r.message.type === "compared" && (r.message.findings ?? 0) > 0)).toBe(true);
  });

  test("a style change through the CSS object model hides it before any heartbeat", async ({ page }) => {
    const { recorded, frame } = await open(page, {
      page: drawing(`setTimeout(() => {
        const sheet = document.createElement("style");
        document.head.appendChild(sheet);
        window.tamperedAt = Date.now();
        sheet.sheet.insertRule("#v::after { content: '9' }");
      }, 2000);`),
    });
    await waitVisible(recorded, "the page before its change");
    const tamperedAt = (await pageValue(await frame(), "tamperedAt", "the page's style change")) as number;
    await expect
      .poll(async () => (await recorded()).reports.some((r) => r.at >= tamperedAt && (r.message.findings ?? 0) > 0), {
        message: "the comparison of the style change",
        timeout: WAIT_MS,
      })
      .toBe(true);
    const record = await recorded();
    const { finding } = firstFinding(record, tamperedAt);
    expect(finding.visibleAfter).toBe(false);
  });

  test("a closed shadow root is opened and hidden", async ({ page }) => {
    const { recorded, frame } = await open(page, {
      page: drawing(`setTimeout(() => {
        const box = document.createElement("span");
        document.body.appendChild(box);
        window.tamperedAt = Date.now();
        const root = box.attachShadow({ mode: "closed" });
        root.append("${WRONG}");
        window.opened = box.shadowRoot === root;
      }, 2000);`),
    });
    await waitVisible(recorded, "the page before its shadow root");
    const content = await frame();
    const tamperedAt = (await pageValue(content, "tamperedAt", "the page's shadow root")) as number;
    expect(await pageValue(content, "opened", "the shadow root's mode")).toBe(true);
    await expect
      .poll(async () => (await recorded()).reports.some((r) => r.at >= tamperedAt && (r.message.findings ?? 0) > 0), {
        message: "the comparison of the shadow root",
        timeout: WAIT_MS,
      })
      .toBe(true);
    await page.waitForTimeout(2_500);
    expect(shownSince(await recorded(), tamperedAt)).toBe(false);
  });

  test("an animation started through the Web Animations API ends at once; one held before its start hides it until it ends", async ({
    page,
  }) => {
    // A custom page plays no motion (README, 「顯示時比對的強制」): animate() gets no duration or
    // delay, so the frame stays shown. An animation whose current time a script moves before its
    // start is not clamped, and is a finding while it waits. Both move an element without a value,
    // so the state they end in is clean. (Fading a value itself would get that value replaced with
    // 「—」 for good: README, 「比對程式」.)
    const { recorded, frame } = await open(page, {
      page: drawing(`setTimeout(() => {
        const mark = document.createElement("i");
        mark.textContent = "*";
        document.body.appendChild(mark);
        window.clampedAt = Date.now();
        const clamped = mark.animate([{ transform: "translateX(0)" }, { transform: "translateX(40px)" }], { duration: 3000, delay: 500, fill: "forwards" });
        const timing = clamped.effect.getTiming();
        window.clamped = { duration: timing.duration, delay: timing.delay, fill: timing.fill, transform: getComputedStyle(mark).transform };
        setTimeout(() => {
          window.tamperedAt = Date.now();
          const held = mark.animate([{ transform: "translateX(0)" }, { transform: "translateX(20px)" }], { fill: "backwards" });
          held.currentTime = -3000;
        }, 2000);
      }, 2000);`),
    });
    await waitVisible(recorded, "the page before its animations");
    const content = await frame();
    const clampedAt = (await pageValue(content, "clampedAt", "the clamped animation")) as number;
    expect(await pageValue(content, "clamped", "the clamped animation's timing")).toEqual({
      duration: 0,
      delay: 0,
      fill: "forwards",
      transform: "matrix(1, 0, 0, 1, 40, 0)",
    });
    const tamperedAt = (await pageValue(content, "tamperedAt", "the animation held before its start")) as number;
    const before = await recorded();
    expect(
      before.reports.filter((r) => r.at >= clampedAt && r.at < tamperedAt && (r.message.findings ?? 0) > 0),
      "no finding for the clamped animation",
    ).toEqual([]);
    expect(
      before.visibility.filter((entry) => entry.at >= clampedAt && entry.at < tamperedAt && !entry.visible),
    ).toEqual([]);
    await expect
      .poll(async () => (await recorded()).reports.some((r) => r.at >= tamperedAt && (r.message.findings ?? 0) > 0), {
        message: "the comparison of the held animation",
        timeout: WAIT_MS,
      })
      .toBe(true);
    const { finding } = firstFinding(await recorded(), tamperedAt);
    expect(finding.visibleAfter).toBe(false);
    expectComparedForTheChange(await recorded(), tamperedAt);
    // Once it has finished, the page compares clean again and is shown: the hidden frame went on
    // rendering, so the animation could finish (CustomPageFrame.tsx, applyView).
    await waitVisible(recorded, "the page after its animation");
    expect(await (await frame()).evaluate(() => document.getElementById("v")?.textContent)).toBe(RIGHT);
  });
});

test.describe("closed on a failure signal", () => {
  test("a page that keeps writing a value back is closed for good", async ({ page }) => {
    const { recorded, frame } = await open(page, {
      page: drawing(`setTimeout(() => {
        window.tamperedAt = Date.now();
        const v = document.getElementById("v");
        new MutationObserver(() => { if (v.textContent !== "${WRONG}") v.textContent = "${WRONG}"; }).observe(v, { childList: true, characterData: true, subtree: true });
        v.textContent = "${WRONG}";
      }, 2000);`),
    });
    await waitVisible(recorded, "the page before it fights");
    const tamperedAt = (await pageValue(await frame(), "tamperedAt", "the page's first rewrite")) as number;
    await expect(page.locator(`iframe[title="${TITLE}"]`), "the frame removed").toHaveCount(0, { timeout: WAIT_MS });
    const record = await recorded();
    expect(shownSince(record, tamperedAt)).toBe(false);
    expect(record.reports.some((r) => r.message.type === "failed")).toBe(true);
    expect(await noticeText(page)).toMatch(/kept putting back values|kept failing the check/);
  });

  test("a page that blocks its thread is hidden when its reports stop, and closed at the heartbeat timeout", async ({
    page,
  }) => {
    const { recorded } = await open(page, {
      page: drawing(`setTimeout(() => {
        window.tamperedAt = Date.now();
        // The wrong value never reaches the screen: the task never ends, so nothing paints.
        document.getElementById("v").textContent = "${WRONG}";
        for (;;) {}
      }, 3000);`),
    });
    await waitVisible(recorded, "the page before it blocks");
    const shownAt = Date.now();
    // When the frame left the outer document, by the outer page's clock.
    const removedAt = (await (
      await page.waitForFunction(
        (title) => (document.querySelector(`iframe[title="${title}"]`) === null ? Date.now() : false),
        TITLE,
        { timeout: WAIT_MS, polling: 50 },
      )
    ).jsonValue()) as number;
    // The outer page kept running all along: it is its own process.
    expect(await page.evaluate(() => 1 + 1)).toBe(2);
    expect(await noticeText(page)).toMatch(/stopped responding/);
    const record = await recorded();
    const lastReport = record.reports.filter((r) => r.at < removedAt).at(-1);
    const lastBeat = record.reports.filter((r) => r.message.type === "beat").at(-1);
    const hidden = record.visibility.find((entry) => entry.at >= shownAt && !entry.visible);
    // Hidden once no report came for STALE_AFTER_MS (the outer page's own timer: README,
    // 「顯示時比對的強制」), before the frame was closed ...
    expect(hidden).toBeDefined();
    expect((hidden?.at ?? 0) - (lastReport?.at ?? 0)).toBeGreaterThanOrEqual(STALE_AFTER_MS - 50);
    expect(hidden?.at ?? Number.POSITIVE_INFINITY).toBeLessThan(removedAt);
    expect(shownSince(record, hidden?.at ?? 0)).toBe(false);
    // ... and closed at least one heartbeat timeout after the last beat: the timeout, not something else.
    expect(removedAt - (lastBeat?.at ?? 0)).toBeGreaterThanOrEqual(HEARTBEAT_TIMEOUT_MS - 50);
  });

  test("a change it is told of, then a blocked thread: hidden in the report of the change", async ({ page }) => {
    const { recorded } = await open(page, {
      page: drawing(`setTimeout(() => {
        window.tamperedAt = Date.now();
        document.getElementById("v").textContent = "${WRONG}";
        // A later task: the change is compared and reported first, then nothing runs again.
        setTimeout(() => { for (;;) {} }, 0);
      }, 3000);`),
    });
    await waitVisible(recorded, "the page before it blocks");
    const shownAt = Date.now();
    await expect(page.locator(`iframe[title="${TITLE}"]`), "the frame removed").toHaveCount(0, { timeout: WAIT_MS });
    const record = await recorded();
    const { finding } = firstFinding(record, shownAt);
    expect(finding.visibleAfter).toBe(false);
    expect(shownSince(record, finding.at)).toBe(false);
    expect(await noticeText(page)).toMatch(/stopped responding/);
  });

  for (const [name, navigation] of [
    ["reloads itself", "location.reload()"],
    ["navigates to the same sandbox document", "location.href = '/sandbox/v1/sandbox.html?again'"],
    ["navigates to the SPA", "location.href = '/settings'"],
    ["navigates to an address on another host", "location.href = 'https://example.com/'"],
  ] as const) {
    test(`a page that ${name} is closed`, async ({ page }) => {
      const { recorded } = await open(page, { page: drawing(`setTimeout(() => { ${navigation}; }, 2000);`) });
      await waitVisible(recorded, "the page before it navigates");
      await expect(page.locator(`iframe[title="${TITLE}"]`), "the frame removed").toHaveCount(0, { timeout: WAIT_MS });
      const record = await recorded();
      expect(record.navigations.length).toBeGreaterThan(0);
      expect(await noticeText(page)).toMatch(/tried to leave its frame/);
      expect(shownSince(record, record.navigations[0] ?? 0)).toBe(false);
    });
  }
});

test.describe("changes the page makes without telling anyone (README, 「顯示時比對的強制」)", () => {
  /** A style the page turns on and off, which replaces the first value with WRONG while it is on. */
  const FAKE_STYLE = `<style id="fake">#v { font-size: 0 } #v::after { content: "${WRONG}"; font-size: 1rem }</style>`;
  /** Whether WRONG is what the page paints for the first value. */
  const FAKE_PAINTED = `() => getComputedStyle(document.getElementById("v"), "::after").content.includes("${WRONG}")`;

  test("turning a style element on through `disabled` hides the frame in the report of that change", async ({
    page,
  }) => {
    const { recorded, frame } = await open(page, {
      page: `${FAKE_STYLE}${drawing(`${WATCH_PAINT}
        document.getElementById("fake").disabled = true;
        watchPaint(${FAKE_PAINTED});
        setTimeout(() => { window.tamperedAt = Date.now(); document.getElementById("fake").disabled = false; }, 3000);`)}`,
    });
    await waitVisible(recorded, "the page before its style change");
    const tamperedAt = (await pageValue(await frame(), "tamperedAt", "the style change")) as number;
    const paintLog = await followPaintLog(page, 3_000);
    const record = await recorded();
    const { finding } = firstFinding(record, tamperedAt);
    expect(finding.visibleAfter).toBe(false);
    expectComparedForTheChange(record, tamperedAt);
    const seen = exposure(record, paintLog, Date.now());
    note("disabled", seen);
    expect(seen.beyondReport).toBe(0);
  });

  test("turning an SVG `<style>` on through `disabled` hides the frame in the report of that change", async ({
    page,
  }) => {
    // SVGStyleElement has a `disabled` of its own, apart from HTMLStyleElement's.
    const { recorded, frame } = await open(page, {
      page: `<svg width="0" height="0">${FAKE_STYLE}</svg>${drawing(`${WATCH_PAINT}
        document.getElementById("fake").disabled = true;
        watchPaint(${FAKE_PAINTED});
        setTimeout(() => { window.tamperedAt = Date.now(); document.getElementById("fake").disabled = false; }, 3000);`)}`,
    });
    await waitVisible(recorded, "the page before its style change");
    const content = await frame();
    expect(await content.evaluate(() => document.getElementById("fake")?.constructor.name)).toBe("SVGStyleElement");
    const tamperedAt = (await pageValue(content, "tamperedAt", "the style change")) as number;
    const paintLog = await followPaintLog(page, 3_000);
    const record = await recorded();
    const { finding } = firstFinding(record, tamperedAt);
    expect(finding.visibleAfter).toBe(false);
    expectComparedForTheChange(record, tamperedAt);
    const seen = exposure(record, paintLog, Date.now());
    note("svg disabled", seen);
    expect(seen.beyondReport).toBe(0);
  });

  for (const [name, cover] of [
    [
      "events it dispatches itself",
      `() => { for (const type of ["scroll", "resize", "focus", "pointermove", "input"]) window.dispatchEvent(new Event(type)); }`,
    ],
    [
      "focus and blur it causes itself",
      `(() => {
        const target = document.createElement("span");
        target.tabIndex = -1;
        document.body.appendChild(target);
        return () => { if (document.activeElement === target) target.blur(); else target.focus(); };
      })()`,
    ],
  ] as const) {
    test(`a wrong value turned on in every frame, under ${name}, is never shown past its report`, async ({ page }) => {
      // The value is on from the page's animation frame callback to the first task after the paint,
      // and every change comes with the cover: the notices the cover gives end with their task
      // (README, 「顯示時比對的強制」), so they explain none of the changes. The page logs each change
      // itself, in the paint log's form: it is on through every paint after an `on`.
      const { recorded, frame } = await open(page, {
        page: `${FAKE_STYLE}${drawing(`window.paintLog = [];
          const fake = document.getElementById("fake");
          fake.disabled = true;
          const cover = ${cover};
          const channel = new MessageChannel();
          const off = () => { fake.disabled = true; cover(); const at = Date.now(); window.paintLog.push([at, false, at]); };
          channel.port1.onmessage = () => { off(); requestAnimationFrame(on); };
          const on = () => { fake.disabled = false; cover(); const at = Date.now(); window.paintLog.push([at, true, at]); channel.port2.postMessage(0); };
          setTimeout(() => { window.tamperedAt = Date.now(); requestAnimationFrame(on); }, 3000);`)}`,
      });
      await waitVisible(recorded, "the page before its changes");
      const tamperedAt = (await pageValue(await frame(), "tamperedAt", "the first change")) as number;
      const paintLog = await followPaintLog(page, 8_000);
      const record = await recorded();
      const seen = exposure(record, paintLog, Date.now());
      note(`every frame, ${name}`, { ...seen, wrongPaints: paintLog.filter(([, wrong]) => wrong).length });
      expect(paintLog.filter(([, wrong]) => wrong).length).toBeGreaterThan(0);
      expect(firstFinding(record, tamperedAt).finding.visibleAfter).toBe(false);
      expect(seen.beyondReport).toBe(0);
      expect(shownSince(record, tamperedAt + 1_000)).toBe(false);
    });
  }

  test("a constructed style sheet, the only kind `adoptedStyleSheets` takes, cannot be made", async ({ page }) => {
    const { recorded, frame } = await open(page, {
      page: drawing(`setTimeout(() => {
        try {
          const sheet = new CSSStyleSheet();
          sheet.replaceSync("#v::after { content: '9' }");
          document.adoptedStyleSheets = [sheet];
          window.made = "made";
        } catch (error) { window.made = String(error); }
        try {
          const Sheet = Object.getPrototypeOf(document.styleSheets[0] ?? document.createElement("style").sheet ?? {}).constructor;
          window.again = typeof Sheet === "function" ? String(new Sheet()) : "no constructor";
        } catch (error) { window.again = String(error); }
      }, 2000);`),
    });
    await waitVisible(recorded, "the page");
    const content = await frame();
    expect(await pageValue(content, "made", "the constructed sheet")).toMatch(
      /is not available in the dashboard sandbox/,
    );
    expect(await pageValue(content, "again", "the constructor through a prototype")).not.toMatch(
      /object CSSStyleSheet/,
    );
    await page.waitForTimeout(1_000);
    await waitVisible(recorded, "the page after it tried");
    expect(await content.evaluate(() => document.getElementById("v")?.textContent)).toBe(RIGHT);
    expect(await content.evaluate(() => document.adoptedStyleSheets.length)).toBe(0);
  });

  test("a page that turns a wrong value on for moments, at any phase, is never shown with it, and is closed", async ({
    page,
  }) => {
    // Each moment is 60 ms, with a pause of 300 to 1300 ms: shorter than any heartbeat gap the page
    // could aim between. The paint log reads the page's own state, not what the runtime hides in the
    // frame, so it counts every paint of WRONG while the outer page shows the frame.
    const { recorded, frame } = await open(page, {
      page: `${FAKE_STYLE}${drawing(`${WATCH_PAINT}
        const fake = document.getElementById("fake");
        fake.disabled = true;
        watchPaint(${FAKE_PAINTED});
        let moments = 0;
        const blink = () => {
          if (moments++ >= 20) return;
          fake.disabled = false;
          setTimeout(() => { fake.disabled = true; setTimeout(blink, 300 + Math.random() * 1000); }, 60);
        };
        setTimeout(() => { window.tamperedAt = Date.now(); blink(); }, 3000);`)}`,
    });
    await waitVisible(recorded, "the page before it blinks");
    await pageValue(await frame(), "tamperedAt", "the first moment");
    const paintLog = await followPaintLog(page, 30_000);
    const record = await recorded();
    const seen = exposure(record, paintLog, Date.now());
    note("dodge", { ...seen, wrongPaints: paintLog.filter(([, wrong]) => wrong).length });
    // README, 「顯示時比對的強制」: every moment is reported, and none is shown past its report.
    expect(seen.beyondReport).toBe(0);
    expect(paintLog.filter(([, wrong]) => wrong).length).toBeGreaterThan(0);
    // The page loses its shown state on each moment it is caught, and is closed on the one after
    // MAX_REGRESSIONS of them (guard.ts).
    await expect(page.locator(`iframe[title="${TITLE}"]`), "the frame removed").toHaveCount(0, { timeout: WAIT_MS });
    expect(await noticeText(page)).toMatch(/kept failing the check/);
    const after = await recorded();
    const tamperedAt = paintLog.find(([, wrong]) => wrong)?.[0] ?? 0;
    const hides = after.visibility.filter(
      (entry, i) => entry.at >= tamperedAt && !entry.visible && after.visibility[i - 1]?.visible,
    );
    expect(hides.length).toBeLessThanOrEqual(MAX_REGRESSIONS + 1);
  });

  test("a CSS animation declared at load in a style attribute, with no script, is never shown while it changes a value", async ({
    page,
  }) => {
    // sandbox.css stops every animation a style sheet declares; a style attribute's `!important`
    // still wins over it, and what such an animation does is the fallback measured here. It starts
    // 3 s after load and, through a custom property the first value's `::after` reads, adds WRONG
    // to it for 1 s: no script runs when it does. A running or pending animation is a finding
    // (README, 「比對程式」), so the frame is not shown while one is pending or running.
    const { recorded, frame } = await open(page, {
      page: `<style>
        @keyframes swap { from { --wrong: " ${WRONG}" } to { --wrong: " ${WRONG}" } }
        #v::after { content: var(--wrong, "") }
      </style><div style="animation: swap 1s 3s 1 !important">${drawing(`${WATCH_PAINT} watchPaint(${FAKE_PAINTED});`)}</div>`,
    });
    const content = await frame();
    await pageValue(content, "paintLog", "the paint log");
    const paintLog = await followPaintLog(page, 7_000);
    const record = await recorded();
    const seen = exposure(record, paintLog, Date.now());
    note("keyframes", { ...seen, wrongPaints: paintLog.filter(([, wrong]) => wrong).length });
    expect(
      paintLog.some(([, wrong]) => wrong),
      "the animation painted WRONG",
    ).toBe(true);
    expect(seen.total).toBe(0);
    expect(record.reports.some((r) => r.message.type === "failed")).toBe(false);
  });

  test("a delayed transition in a style attribute that a hover starts is never shown while it changes a value", async ({
    page,
  }) => {
    // `:hover` and a discrete transition with a delay, in a style attribute's `!important` (which
    // wins over sandbox.css): WRONG appears 1 s after the pointer arrives, through a custom property
    // the first value's `::after` reads, with no script at all.
    const { recorded, frame } = await open(page, {
      page: `<style>
        .w:hover { --wrong: " ${WRONG}" }
        #v::after { content: var(--wrong, "") }
      </style><div class="w" style="transition: --wrong 0s 1s allow-discrete !important">${drawing(`${WATCH_PAINT} watchPaint(${FAKE_PAINTED});`)}</div>`,
    });
    await waitVisible(recorded, "the page before the hover");
    const content = await frame();
    await pageValue(content, "paintLog", "the paint log");
    const hoverAt = Date.now();
    await content.locator("p").hover();
    const paintLog = await followPaintLog(page, 3_000);
    const record = await recorded();
    const seen = exposure(record, paintLog, Date.now());
    note("hover transition", { ...seen, wrongPaints: paintLog.filter(([, wrong]) => wrong).length });
    expect(
      paintLog.some(([, wrong]) => wrong),
      "the transition painted WRONG",
    ).toBe(true);
    // The hover is reported at once (a poke), before the transition's delay ends.
    expect(firstFinding(record, hoverAt).finding.visibleAfter).toBe(false);
    expect(seen.total).toBe(0);
    expect(seen.beyondReport).toBe(0);
  });

  test("an SVG geometry write, which Chromium does not report as a mutation, hides it in the report of that write", async ({
    page,
  }) => {
    // A bar whose width is the first power value, made wrong through the SVG DOM's `baseVal`.
    const { recorded, frame } = await open(page, {
      page: `<svg width="300" height="60"><rect id="bar0" x="0" y="0" height="10"></rect><rect id="bar1" x="0" y="20" height="10"></rect><rect id="bar2" x="0" y="40" height="10"></rect></svg>
<script>
  ${WATCH_PAINT}
  let right = null;
  tq.on("data", (tq) => {
    const rows = tq.dataset("power").rows;
    for (const i of [0, 1, 2]) {
      const bar = document.getElementById("bar" + i);
      bar.setAttribute("width", String(rows[i].kw * 4));
      tq.mark(bar, "c|power|kw|" + i, { chart: "p", channel: "width" });
    }
    right = rows[0].kw * 4;
  });
  // (The SVG DOM keeps lengths as 32-bit floats.)
  watchPaint(() => right !== null && Math.abs(document.getElementById("bar0").width.baseVal.value - right) > 0.5);
  setTimeout(() => { window.tamperedAt = Date.now(); document.getElementById("bar0").width.baseVal.value = 290; }, 3000);
</script>`,
    });
    await waitVisible(recorded, "the page before the write");
    const tamperedAt = (await pageValue(await frame(), "tamperedAt", "the SVG write")) as number;
    const paintLog = await followPaintLog(page, 2_000);
    const record = await recorded();
    const { finding } = firstFinding(record, tamperedAt);
    expect(finding.visibleAfter).toBe(false);
    expectComparedForTheChange(record, tamperedAt);
    const seen = exposure(record, paintLog, Date.now());
    note("svg", seen);
    expect(seen.beyondReport).toBe(0);
  });
});

test.describe("no motion (README, 「顯示時比對的強制」)", () => {
  /** How many of the frame's animations are running or paused. */
  const live = (content: Frame) =>
    content.evaluate(
      () => document.getAnimations().filter((a) => a.playState === "running" || a.playState === "paused").length,
    );

  /** Every hide of the frame at or after `from`, after it had been shown. */
  const hidesSince = (record: Recorded, from: number) =>
    record.visibility.filter((entry, i) => entry.at >= from && !entry.visible && record.visibility[i - 1]?.visible);

  /** Waits until the runtime has reported `count` comparisons after `from`. */
  async function comparedSince(recorded: () => Promise<Recorded>, from: number, count: number, what: string) {
    await expect
      .poll(
        async () => (await recorded()).reports.filter((r) => r.at >= from && r.message.type === "compared").length,
        { message: `comparisons reported after ${what}`, timeout: WAIT_MS },
      )
      .toBeGreaterThanOrEqual(count);
  }

  test("a page with a :hover transition and a :hover animation is never hidden over ten hovers, and the hover style applies at once", async ({
    page,
  }) => {
    // A legitimate page: a card that changes colour on hover with a transition, a badge that
    // animates in on hover with a forwards fill, and a script animation on pointerenter. Without
    // motion each applies at once, so no comparison finds a live animation, and the outer page never
    // loses its shown state (guard.ts, MAX_REGRESSIONS).
    const { recorded, frame } = await open(page, {
      page: `<style>
        .card { padding: 8px; background-color: rgb(238, 238, 238); transition: background-color 400ms ease 100ms, box-shadow 300ms; }
        .card:hover { background-color: rgb(37, 99, 235); box-shadow: 0 0 4px rgb(0, 0, 0); }
        @keyframes pop { from { opacity: 0.2 } to { opacity: 0.9 } }
        .badge { display: inline-block; opacity: 0.5; }
        .card:hover .badge { animation: pop 400ms ease-out forwards; }
      </style>
      <div class="card">${drawing(`document.querySelector(".card").addEventListener("pointerenter", (event) => {
        event.currentTarget.animate([{ outlineColor: "rgb(255, 0, 0)" }, { outlineColor: "rgb(0, 0, 255)" }], { duration: 500, fill: "forwards" });
      });`)}<span class="badge">*</span></div>
      <p id="out">elsewhere</p>`,
    });
    await waitVisible(recorded, "the page before the hovers");
    const content = await frame();
    const shownAt = Date.now();
    const look = () =>
      content.evaluate(() => {
        const card = getComputedStyle(document.querySelector(".card") as Element);
        return {
          background: card.backgroundColor,
          outline: card.outlineColor,
          badge: getComputedStyle(document.querySelector(".badge") as Element).opacity,
        };
      });
    for (let i = 0; i < 10; i++) {
      await content.locator(".card").hover();
      expect(await look(), `hover ${i + 1}: the hover style at once`).toEqual({
        background: "rgb(37, 99, 235)",
        outline: "rgb(0, 0, 255)",
        badge: "0.9",
      });
      expect(await live(content), `hover ${i + 1}: nothing running`).toBe(0);
      await content.locator("#out").hover();
      expect(await look(), `hover ${i + 1}: the style without it at once`).toMatchObject({
        background: "rgb(238, 238, 238)",
        badge: "0.5",
      });
    }
    const lastAt = Date.now();
    // The comparisons of the last moves, and the ones after them, have been reported.
    await comparedSince(recorded, lastAt, 3, "the last hover");
    const record = await recorded();
    expect(
      record.reports.filter((r) => r.at >= shownAt && ((r.message.findings ?? 0) > 0 || r.message.type === "failed")),
      "no finding or failure after the frame was shown",
    ).toEqual([]);
    expect(hidesSince(record, shownAt), "never hidden").toEqual([]);
    expect(record.visibility.at(-1)?.visible).toBe(true);
    await expect(page.locator(`iframe[title="${TITLE}"]`)).toHaveCount(1);
  });

  test("the page's own !important, unlayered or in a layer of its own, cannot turn motion back on", async ({
    page,
  }) => {
    // Each of these would keep an animation running, paused, waiting or tied to scrolling, or a
    // transition running, which would keep the frame hidden: the motion layer is declared first and
    // its `!important` wins over every later layer's and every unlayered one.
    const { recorded, frame } = await open(page, {
      page: `<style>
        @layer first, second;
        @keyframes slide { from { transform: translateX(0) } to { transform: translateX(30px) } }
        #u { animation: slide 2s 500ms infinite alternate !important; transition: transform 2s 500ms !important; }
        #u:hover { transform: translateX(10px); }
        @layer first {
          #a { animation: slide 2s 500ms infinite alternate !important; animation-play-state: paused !important; transition-property: all !important; transition-duration: 2s !important; }
          #a:hover { transform: translateX(10px); }
        }
        @layer second {
          #b { animation: slide 2s linear infinite !important; animation-timeline: scroll(root) !important; }
        }
        #u::after { content: "u"; animation: slide 2s infinite !important; }
      </style><div id="u">u</div><div id="a">a</div><div id="b">b</div>${drawing()}`,
    });
    // Shown at all: with any of them running, the page would stay hidden from the start.
    await waitVisible(recorded, "the page with its own !important motion");
    const content = await frame();
    const shownAt = Date.now();
    const computed = () =>
      content.evaluate(() =>
        ["u", "a", "b"].map((id) => {
          const style = getComputedStyle(document.getElementById(id) as Element);
          return [
            id,
            style.animationDuration,
            style.animationDelay,
            style.animationPlayState,
            style.animationTimeline,
            style.transitionProperty,
          ];
        }),
      );
    expect(await computed()).toEqual(["u", "a", "b"].map((id) => [id, "0s", "0s", "running", "auto", "none"]));
    expect(
      await content.evaluate(
        () => getComputedStyle(document.getElementById("u") as Element, "::after").animationDuration,
      ),
    ).toBe("0s");
    const transform = (id: string) =>
      content.evaluate((which) => getComputedStyle(document.getElementById(which) as Element).transform, id);
    // The hover styles apply at once, with no transition.
    await content.locator("#u").hover();
    expect(await transform("u")).toBe("matrix(1, 0, 0, 1, 10, 0)");
    await content.locator("#a").hover();
    expect(await transform("a")).toBe("matrix(1, 0, 0, 1, 10, 0)");
    await content.locator("p").hover();
    expect(await transform("a")).toBe("none");
    await comparedSince(recorded, Date.now(), 3, "the hovers");
    const record = await recorded();
    expect(record.reports.filter((r) => r.at >= shownAt && (r.message.findings ?? 0) > 0)).toEqual([]);
    expect(hidesSince(record, shownAt)).toEqual([]);
    expect(await live(content)).toBe(0);
  });

  /**
   * Moves the pointer onto `.card` and off it (onto `#out`) until the frame is removed, waiting after
   * each move until the frame is hidden and then shown again (or removed); returns how many times
   * the shown frame was hidden after `shownAt`.
   */
  async function hoverUntilClosed(
    page: Page,
    recorded: () => Promise<Recorded>,
    frame: () => Promise<Frame>,
    shownAt: number,
  ) {
    const frames = page.locator(`iframe[title="${TITLE}"]`);
    for (let i = 0; i < 2 * (MAX_REGRESSIONS + 2) && (await frames.count()) > 0; i++) {
      const content = await frame();
      const hoverAt = Date.now();
      await content
        .locator(i % 2 === 0 ? ".card" : "#out")
        .hover({ timeout: WAIT_MS })
        .catch(() => undefined);
      // Hidden (or closed) after the move, then shown again once the transition has ended.
      await expect
        .poll(async () => (await frames.count()) === 0 || hidesSince(await recorded(), hoverAt).length > 0, {
          message: `move ${i + 1}: the frame hidden or closed`,
          timeout: WAIT_MS,
        })
        .toBe(true);
      await expect
        .poll(async () => (await frames.count()) === 0 || (await recorded()).visibility.at(-1)?.visible === true, {
          message: `move ${i + 1}: the frame shown again or closed`,
          timeout: WAIT_MS,
        })
        .toBe(true);
    }
    await expect(frames, "the frame removed").toHaveCount(0, { timeout: WAIT_MS });
    expect(await noticeText(page)).toMatch(/kept failing the check/);
    return hidesSince(await recorded(), shownAt).length;
  }

  /** A card whose `.dot` moves on hover: with motion, a transition the comparison finds. */
  const HOVER_MOVES = `<style>
        .dot { display: inline-block; }
        .card:hover .dot { transform: translateX(10px); }
      </style>`;

  test("a transition in a style attribute's !important still hides the frame on each hover, and is closed past MAX_REGRESSIONS", async ({
    page,
  }) => {
    // A style attribute's `!important` wins over sandbox.css: the transition runs, the comparison
    // finds it (README, 「顯示時比對的強制」), and each time it hides the shown frame is a regression.
    const { recorded, frame } = await open(page, {
      page: `${HOVER_MOVES}<div class="card">${drawing()}<span class="dot" style="transition: transform 300ms !important">*</span></div>
      <p id="out">elsewhere</p>`,
    });
    await waitVisible(recorded, "the page before the hovers");
    const hides = await hoverUntilClosed(page, recorded, frame, Date.now());
    note("inline transition", { hides });
    expect(hides).toBe(MAX_REGRESSIONS + 1);
  });

  test("a sheet a page script puts ahead of sandbox.css, with a layer of its own, still hides the frame on each hover, and is closed past MAX_REGRESSIONS", async ({
    page,
  }) => {
    // The page's own anonymous layer, in a <style> its script inserts before sandbox.css, is
    // declared first, so its `!important` transition wins over the motion layer; the transition runs
    // and the comparison finds it, as for a style attribute's.
    const { recorded, frame } = await open(page, {
      page: `${HOVER_MOVES}<div class="card">${drawing(`const ahead = document.createElement("style");
        ahead.textContent = "@layer { .dot { transition: transform 300ms !important; } }";
        document.head.prepend(ahead);`)}<span class="dot">*</span></div>
      <p id="out">elsewhere</p>`,
    });
    await waitVisible(recorded, "the page before the hovers");
    const content = await frame();
    expect(
      await content.evaluate(() => [
        document.head.firstElementChild?.localName,
        getComputedStyle(document.querySelector(".dot") as Element).transitionProperty,
      ]),
      "the page's sheet is first and its transition wins",
    ).toEqual(["style", "transform"]);
    const hides = await hoverUntilClosed(page, recorded, frame, Date.now());
    note("sheet ahead of sandbox.css", { hides });
    expect(hides).toBe(MAX_REGRESSIONS + 1);
  });

  test("a style attribute's animation with a negative delay, which ends at 0 as one without motion does, is counted and hides the frame", async ({
    page,
  }) => {
    // `animation: slide 10s -10s both !important` in a style attribute escapes the motion layer and
    // ends at time 0, as an animation without motion does, but it has a delay and an active
    // duration, so the comparison counts it (dom.ts, `ended`). Set by a script, it starts at the next
    // style update, so the comparison at the end of that task reads it pending at time 0 and rate 1.
    // reverse() afterwards is measured only (`note`): Chromium resolves the reversed animation, now
    // finished, before the comparison of that task reads it, so it is neither running nor pending
    // there and `ended` is not reached (were it pending, playing backwards, it would count).
    const { recorded, frame } = await open(page, {
      page: `<style>@keyframes slide { from { transform: translateX(0) } to { transform: translateX(30px) } }</style>
      ${drawing()}<span id="neg" style="display: inline-block">*</span>`,
    });
    await waitVisible(recorded, "the page before the animation");
    const content = await frame();
    const startAt = Date.now();
    await content.evaluate(() =>
      (document.getElementById("neg") as HTMLElement).style.setProperty(
        "animation",
        "slide 10s -10s both",
        "important",
      ),
    );
    await expect
      .poll(
        async () => (await recorded()).reports.filter((r) => r.at >= startAt && (r.message.findings ?? 0) > 0).length,
        { message: "a finding reported after the animation was set", timeout: WAIT_MS },
      )
      .toBeGreaterThan(0);
    await expect
      .poll(async () => hidesSince(await recorded(), startAt).length, {
        message: "the frame hidden after the animation was set",
        timeout: WAIT_MS,
      })
      .toBeGreaterThan(0);
    const timing = await content.evaluate(() => {
      const animation = document.getAnimations()[0] as Animation;
      const { delay, activeDuration, endTime } = (animation.effect as AnimationEffect).getComputedTiming();
      return { count: document.getAnimations().length, delay, activeDuration, endTime };
    });
    expect(timing, "one animation that ends at 0 with a negative delay").toEqual({
      count: 1,
      delay: -10000,
      activeDuration: 10000,
      endTime: 0,
    });
    await waitVisible(recorded, "the page after the animation finished");
    const reverseAt = Date.now();
    const reversed = await content.evaluate(() => {
      const animation = document.getAnimations()[0] as Animation;
      animation.reverse();
      return { pending: animation.pending, rate: animation.playbackRate, time: animation.currentTime };
    });
    await comparedSince(recorded, reverseAt, 2, "reverse()");
    const record = await recorded();
    note("negative delay", {
      hidesAfterStart: hidesSince(record, startAt).filter((entry) => entry.at < reverseAt).length,
      reversed,
      findingsAfterReverse: record.reports.filter((r) => r.at >= reverseAt && (r.message.findings ?? 0) > 0).length,
    });
  });
});

test.describe("messages the page sends", () => {
  test("forged and resent window messages show nothing", async ({ page }) => {
    const { recorded, frame } = await open(page, {
      page: `<p><b id="v" data-tq="c|power|kw|0">${WRONG}</b></p>
<script>
  const forged = [
    { tq: 1, type: "compared", generation: 1, findings: 0 },
    { tq: 1, type: "beat", seq: 1 },
    { tq: 1, type: "ready", runtime: 1 },
    { tq: 1, type: "rendered", generation: 1 },
    { tq: 1, type: "resize", height: 900 },
  ];
  let rounds = 0;
  const send = () => {
    for (const message of forged) parent.postMessage(message, "*");
    if (++rounds < 200) setTimeout(send, 10); else window.sent = rounds;
  };
  send();
  tq.on("data", () => {});
</script>`,
    });
    expect(await pageValue(await frame(), "sent", "the forged messages")).toBe(200);
    await page.waitForTimeout(2_500);
    const record = await recorded();
    expect(
      record.visibility.some((entry) => entry.visible),
      "shown at some moment",
    ).toBe(false);
    // Only the port's reports arrived: no forged `resize` of 900.
    expect(
      record.reports.some((r) => r.message.type === "resize" && (r.message as { height?: number }).height === 900),
    ).toBe(false);
  });

  test("a flood of messages and mutations does not keep a changed value on screen", async ({ page }) => {
    const { recorded, frame } = await open(page, {
      page: drawing(`setTimeout(() => {
        let n = 0;
        const flood = () => {
          for (let i = 0; i < 2000; i++) { parent.postMessage({ tq: 1, type: "compared", generation: 1, findings: 0 }, "*"); postMessage({ n: i }, "*"); }
          const filler = document.createElement("i");
          document.body.appendChild(filler);
          filler.remove();
          if (++n === 20) { document.getElementById("v").textContent = "${WRONG}"; window.tamperedAt = Date.now(); }
          if (n < 200) setTimeout(flood, 0);
        };
        flood();
      }, 2000);`),
    });
    await waitVisible(recorded, "the page before the flood");
    const tamperedAt = (await pageValue(await frame(), "tamperedAt", "the change during the flood")) as number;
    await expect
      .poll(async () => (await recorded()).reports.some((r) => r.at >= tamperedAt && (r.message.findings ?? 0) > 0), {
        message: "the comparison of the change during the flood",
        timeout: WAIT_MS,
      })
      .toBe(true);
    const { finding } = firstFinding(await recorded(), tamperedAt);
    expect(finding.visibleAfter).toBe(false);
    await page.waitForTimeout(2_500);
    expect(shownSince(await recorded(), finding.at)).toBe(false);
  });
});
