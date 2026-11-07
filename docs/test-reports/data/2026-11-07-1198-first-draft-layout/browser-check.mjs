// The first-draft layout walkthrough of the demo script's step 3 (Laysi/trustiot-v2#1198), run in the
// Playwright image (frontend/e2e/image.ts) against the UI preview (`vite --mode ui-preview`).
// The presenter types the script's main request on the home page with ` !draft` appended (the
// preview's keyword that makes the answer write a draft, frontend/README.md), sends it, and watches
// the new conversation while the answer streams and is stored. The page is one column until the
// answer is stored and the drafts list is read again; then the draft panel appears (two columns from
// `xl`, 1280 px). Nothing is scrolled by hand: the scroll position is only recorded.
//
// Locale zh-TW, Asia/Taipei, light theme, zoom 100 %; CSS viewports 1280×720, 1536×768, 1920×1080
// and, as the control where the page stays one column, 1279×720. What only the preview draws (its
// toolbar, the composer's completion hint and template button) is hidden as in the Laysi/trustiot-v2#1195
// walkthrough, so the numbers are the product's layout.
//
// A probe is installed before the app starts (`probeInstall`): from the moment of sending it samples
// the page on every animation frame (`requestAnimationFrame`), watches the DOM with a
// `MutationObserver` and layout shifts with a `PerformanceObserver`, and logs the requests the app
// builds. The script waits on conditions (the answer stored, the card drawn, the layout unchanged for
// a number of frames), never on a fixed sleep. Each run prints JSON lines (`step`), derived from the
// frames; the frames are kept in `frames` lines only with FRAMES=1. Screenshots are never part of the
// record: with SHOTS=1 a CDP screencast of the run prints the frames around the switch, base64,
// between markers, for the reviewer's eyes.
//
// Usage: docs/test-reports/data/2026-11-07-1198-first-draft-layout/run.sh (from the repository root).
// TREE=<label> names the tree measured. VIEWPORTS=<id,id> limits the run to some viewports,
// REPEATS=<n> repeats each viewport (default 2, with fresh pages), SHOTS=1 takes the screencast in
// the last repeat of each viewport, FRAMES=1 prints the sampled frames of every run.
import { createRequire } from "node:module";

const { chromium } = createRequire("/w/frontend/package.json")("@playwright/test");
const BASE = process.env.PREVIEW_URL ?? "http://127.0.0.1:5198";
const TREE = process.env.TREE ?? "unlabelled";
/** Generous: the build host is shared and often loaded (CLAUDE.md, 「Test time limits」). */
const WAIT_MS = 90_000;
/** The script's step 3 request, with the preview's keyword appended. */
const REQUEST =
  "做一個儀表板：最近 7 天每天的平均溫度與最高溫度，用長條圖比較，另外用一個數字顯示現在的溫度和濕度。以台北時間分天。 !draft";
const QUESTION_START = "做一個儀表板";
/** The preview's example answer to a `!draft` question (frontend/src/test/fake-questions.ts), by the start of each paragraph. */
const PARAGRAPHS = ["This is a fictional answer", "The draft compares", "The draft is below this answer"];
/** Frames with an unchanged layout that count as settled: half a second at 60 frames a second. */
const SETTLED_FRAMES = 30;

const ALL = [
  { id: "1280x720", width: 1280, height: 720, dsf: 1, scheme: "light" },
  { id: "1536x768", width: 1536, height: 768, dsf: 1, scheme: "light" },
  { id: "1920x1080", width: 1920, height: 1080, dsf: 1, scheme: "light" },
  { id: "1279x720-control", width: 1279, height: 720, dsf: 1, scheme: "light" },
];
const only = (process.env.VIEWPORTS ?? "").split(",").filter(Boolean);
const VIEWPORTS = only.length > 0 ? ALL.filter((viewport) => only.includes(viewport.id)) : ALL;
const REPEATS = Math.max(1, Number(process.env.REPEATS ?? 2));
const TAKE_SHOTS = process.env.SHOTS === "1";
/** The preview toolbar's slow setting (0, 2000 or 10000 ms): every API request waits this long, standing for a network round trip. */
const LATENCY_MS = Number(process.env.LATENCY_MS ?? 0);
const KEEP_FRAMES = process.env.FRAMES === "1";

const browser = await chromium.launch();
const failures = [];
const shots = [];

function emit(where, step, data) {
  console.log(JSON.stringify({ step, tree: TREE, ...where, ...data }));
}

/**
 * Runs in every page before the app: hides what the UI preview draws and the product does not, and
 * installs the probe (see the header). `window.__probe.start()` begins the sampling.
 */
function probeInstall() {
  const hidden = new Set();
  const applyHiding = () => {
    for (const box of document.querySelectorAll("textarea[aria-describedby]")) {
      for (const id of box.getAttribute("aria-describedby").split(/\s+/)) {
        const hint = document.getElementById(id);
        if (!hint) continue;
        for (const node of hint.childNodes) {
          if (node.nodeType === 3 && node.nodeValue.includes("灰字")) {
            node.nodeValue = "";
            hidden.add("completion-hint");
          }
        }
      }
    }
  };
  document.addEventListener("DOMContentLoaded", () => {
    const style = document.createElement("style");
    style.textContent = ["#ui-preview-toolbar{display:none!important}", 'button[aria-label="問句範本（/）"]{display:none!important}'].join("\n");
    document.head.append(style);
    new MutationObserver(applyHiding).observe(document.body, { subtree: true, childList: true, characterData: true });
    applyHiding();
  });

  const probe = {
    running: false,
    frames: [],
    events: [],
    shifts: [],
    requests: [],
    hidden: () => [...hidden],
    unchangedFrames: 0,
    timeOrigin: performance.timeOrigin,
    t0: 0,
  };
  window.__probe = probe;
  const now = () => performance.now();
  const ids = new WeakMap();
  let nextId = 1;
  const idOf = (element) => {
    if (!ids.has(element)) ids.set(element, nextId++);
    return ids.get(element);
  };
  const box = (element) => {
    if (!element) return null;
    const rect = element.getBoundingClientRect();
    const round = (value) => Math.round(value * 10) / 10;
    return { x: round(rect.left), y: round(rect.top), w: round(rect.width), h: round(rect.height) };
  };
  const label = (element) => {
    if (!element) return null;
    if (element.nodeType !== 1) return `#text in ${element.parentElement ? label(element.parentElement) : "?"}`;
    const own = element.getAttribute("aria-label") ?? element.getAttribute("data-testid") ?? (element.id ? `#${element.id}` : "");
    return `${element.tagName.toLowerCase()}${own ? `[${own}]` : ""}${element.className && typeof element.className === "string" ? `.${element.className.split(/\s+/).slice(0, 3).join(".")}` : ""}`;
  };

  // The requests the app builds (its API client calls `new Request(url, init)`; a Request made from a
  // Request is a copy and not counted). The preview answers them in the page, so there is no network
  // entry to read: the time a request is built stands for the time it is sent.
  const BaseRequest = window.Request;
  window.Request = class extends BaseRequest {
    constructor(input, init) {
      super(input, init);
      if (!probe.running || input instanceof BaseRequest) return;
      try {
        const url = new URL(this.url);
        if (url.pathname.startsWith("/api/")) probe.requests.push({ t: now(), method: this.method, path: url.pathname + url.search });
      } catch {
        // Not a URL the page can read: not an API request.
      }
    }
  };

  const INTEREST = 'section[data-answer-dashboard], .content-start.bg-muted.rounded-xl, [aria-busy="true"], [role="alert"], canvas, article, ol[aria-label]';
  const describeNode = (node) => {
    const name = label(node);
    return node.matches('canvas') ? `canvas#${idOf(node)}` : name;
  };
  const observeMutations = () =>
    new MutationObserver((records) => {
      if (!probe.running) return;
      const t = now();
      for (const record of records) {
        if (record.type === "attributes") {
          probe.events.push({ t, op: "class", what: label(record.target), to: record.target.className.toString().slice(0, 140) });
          continue;
        }
        for (const [op, list] of [
          ["add", record.addedNodes],
          ["rm", record.removedNodes],
        ]) {
          for (const node of list) {
            if (node.nodeType !== 1) continue;
            const found = [...(node.matches(INTEREST) ? [node] : []), ...node.querySelectorAll(INTEREST)];
            for (const item of found) probe.events.push({ t, op, what: describeNode(item) });
          }
        }
      }
    });
  const watch = () => {
    const root = document.getElementById("root");
    if (!root) return false;
    const observer = observeMutations();
    observer.observe(root, { subtree: true, childList: true, attributes: true, attributeFilter: ["class"], attributeOldValue: false });
    return true;
  };
  document.addEventListener("DOMContentLoaded", () => {
    if (!watch()) addEventListener("load", watch);
  });
  document.addEventListener("focusin", (event) => {
    if (probe.running) probe.events.push({ t: now(), op: "focus", what: label(event.target) });
  });
  try {
    new PerformanceObserver((list) => {
      if (!probe.running) return;
      for (const entry of list.getEntries()) {
        probe.shifts.push({
          t: Math.round(entry.startTime * 10) / 10,
          value: Math.round(entry.value * 10000) / 10000,
          hadRecentInput: entry.hadRecentInput,
          sources: (entry.sources ?? []).slice(0, 4).map((source) => ({
            node: source.node ? label(source.node) : null,
            from: { x: Math.round(source.previousRect.x), y: Math.round(source.previousRect.y), w: Math.round(source.previousRect.width), h: Math.round(source.previousRect.height) },
            to: { x: Math.round(source.currentRect.x), y: Math.round(source.currentRect.y), w: Math.round(source.currentRect.width), h: Math.round(source.currentRect.height) },
          })),
        });
      }
    }).observe({ type: "layout-shift", buffered: false });
  } catch {
    probe.shiftsUnsupported = true;
  }

  const textOf = (element) => (element.textContent ?? "").replace(/\s+/g, " ").trim();
  const findText = (selector, start) => [...document.querySelectorAll(selector)].find((element) => textOf(element).startsWith(start)) ?? null;
  const questionElement = (start) =>
    [...document.querySelectorAll("#root main li")].find((element) => !element.querySelector("li") && textOf(element).includes(start)) ?? null;

  /** Everything the analysis needs from the page at one frame. */
  const snapshot = (frameTime) => {
    const root = document.getElementById("root");
    const main = root.querySelector("main");
    const vh = window.innerHeight;
    const header = root.querySelector("header.sticky");
    const headerBottom = header ? Math.round(header.getBoundingClientRect().bottom) : 0;
    const textarea = main?.querySelector("textarea");
    // The conversation's composer is the sticky box that holds the textarea.
    const composer = textarea?.closest(".sticky") ?? null;
    const composerTop = composer ? Math.round(composer.getBoundingClientRect().top) : vh;
    const onConversation = location.pathname.startsWith("/conversations/");
    const column = onConversation && textarea ? textarea.closest("div.max-w-3xl") : null;
    const grid = column?.parentElement ?? null;
    const columns = grid ? getComputedStyle(grid).gridTemplateColumns.trim().split(/\s+/).length : 0;
    const panel = main?.querySelector(".content-start.bg-muted.rounded-xl") ?? null;
    const card = main?.querySelector("section[data-answer-dashboard]") ?? null;
    const chartCanvases = card ? [...card.querySelectorAll("canvas")] : [];
    const firstChart = chartCanvases.at(0) ?? null;
    const chartBox = firstChart ? firstChart.closest(".h-72") ?? firstChart.parentElement : null;
    const paragraph = PARAGRAPHS_IN_PAGE.map((start) => findText("#root main article p", start));
    const question = questionElement(QUESTION_IN_PAGE);
    const active = document.activeElement && document.activeElement !== document.body ? document.activeElement : null;
    const place = (element) => {
      const value = box(element);
      if (!value) return null;
      const top = Math.max(value.y, headerBottom);
      const bottom = Math.min(value.y + value.h, composerTop);
      return { ...value, shown: Math.round(Math.max(0, bottom - top)), inBand: value.y >= headerBottom - 1 && value.y + value.h <= composerTop + 1 };
    };
    return {
      t: Math.round(frameTime * 10) / 10,
      path: location.pathname,
      sy: Math.round(window.scrollY * 10) / 10,
      sh: document.scrollingElement.scrollHeight,
      sw: document.scrollingElement.scrollWidth,
      cw: document.documentElement.clientWidth,
      band: { top: headerBottom, bottom: composerTop },
      columns,
      col: box(column),
      panel: panel ? { ...box(panel), skeleton: panel.querySelector("[aria-busy='true']") !== null } : null,
      q: place(question),
      p: paragraph.map(place),
      writing: Boolean(main?.querySelector("article[aria-busy='true']")),
      card: card ? { ...box(card), skeleton: false, h3: place(card.querySelector("h3")), busy: card.querySelectorAll("[aria-busy='true']").length, pulse: card.querySelectorAll(".animate-pulse").length } : null,
      cardLoading: card === null && [...(main?.querySelectorAll("[aria-busy='true']") ?? [])].some((element) => element.tagName === "DIV" && !panel?.contains(element) && element.querySelector(".animate-pulse")),
      chart: chartBox ? place(chartBox) : null,
      canvases: chartCanvases.map((canvas) => ({ id: idOf(canvas), w: canvas.width, h: canvas.height, cw: Math.round(canvas.getBoundingClientRect().width) })),
      alerts: [...(main?.querySelectorAll("[role='alert']") ?? [])].map((element) => textOf(element).slice(0, 80)),
      focus: active ? { what: label(active), ...(place(active) ?? {}) } : null,
    };
  };
  const PARAGRAPHS_IN_PAGE = [];
  let QUESTION_IN_PAGE = "";
  probe.configure = (paragraphs, question) => {
    PARAGRAPHS_IN_PAGE.splice(0, PARAGRAPHS_IN_PAGE.length, ...paragraphs);
    QUESTION_IN_PAGE = question;
  };

  let last = "";
  const loop = (frameTime) => {
    if (!probe.running) return;
    const frame = snapshot(frameTime);
    const key = JSON.stringify({ ...frame, t: 0, sy: frame.sy });
    probe.unchangedFrames = key === last ? probe.unchangedFrames + 1 : 0;
    last = key;
    probe.frames.push(frame);
    requestAnimationFrame(loop);
  };
  probe.start = () => {
    probe.running = true;
    probe.t0 = now();
    probe.frames.length = 0;
    probe.unchangedFrames = 0;
    probe.events.length = 0;
    probe.shifts.length = 0;
    probe.requests.length = 0;
    requestAnimationFrame(loop);
  };
  probe.stop = () => {
    probe.running = false;
  };
  probe.mark = (what) => probe.events.push({ t: now(), op: "mark", what });
}

/** A frame's state as one comparable label: columns, what the card is, the panel. */
function stateKey(frame) {
  const card = frame.card ? (frame.card.busy > 0 || frame.card.pulse > 0 ? "card-loading" : frame.canvases.length > 0 ? "card" : "card-no-chart") : frame.cardLoading ? "card-skeleton" : "no-card";
  const panel = frame.panel ? (frame.panel.skeleton ? "panel-skeleton" : "panel") : "no-panel";
  const wide = frame.columns > 1;
  return `${wide ? "2col" : "1col"}|col${Math.round(frame.col?.w ?? 0)}|${card}|${panel}${frame.alerts.length > 0 ? "|alert" : ""}${frame.writing ? "|writing" : ""}`;
}

const round = (value) => (value === null || value === undefined ? null : Math.round(value * 10) / 10);

/** What the frames say, as the JSON lines of one run. */
function analyse(where, data) {
  const { frames, events, shifts, requests, t0 } = data;
  const at = (predicate) => frames.find(predicate);
  const firstQuestion = at((frame) => frame.q !== null);
  const firstP = PARAGRAPHS.map((_, index) => at((frame) => frame.p[index] !== null));
  // The answer is stored when the thread draws it: its last paragraph is on the page and the writing
  // line is gone. The drafts list is read after that.
  const stored = at((frame) => frame.p[2] !== null);
  const firstCard = at((frame) => frame.card !== null);
  const firstChart = at((frame) => frame.canvases.length > 0);
  const firstPanel = at((frame) => frame.panel !== null);
  const firstWide = at((frame) => frame.columns > 1);
  const last = frames.at(-1);
  const settledAt = (() => {
    // The first frame from which the layout stays as it is at the end.
    const finalKey = stateKey(last);
    for (let index = frames.length - 1; index >= 0; index -= 1) {
      if (stateKey(frames[index]) !== finalKey) return frames[Math.min(index + 1, frames.length - 1)];
    }
    return frames[0];
  })();
  const t = (frame) => (frame && stored ? round(frame.t - stored.t) : null);
  const emitStep = (step, extra) => emit(where, step, extra);

  emitStep("timeline: first appearance, ms from the answer being stored", {
    storedAtMsFromStart: stored ? round(stored.t - t0) : null,
    sentAnd: { questionDrawn: t(firstQuestion), paragraph1: t(firstP[0]), paragraph2: t(firstP[1]), paragraph3AnswerStored: 0, card: t(firstCard), chart: t(firstChart), panel: t(firstPanel), twoColumns: t(firstWide), layoutFinal: t(settledAt) },
    note: "ms relative to the first frame that shows the stored answer's last paragraph; negative is before it",
  });

  // The sequence of layout states from the first frame of the conversation page, compressed.
  const sequence = [];
  for (const frame of frames.filter((item) => item.path.startsWith("/conversations/"))) {
    const key = stateKey(frame);
    const previous = sequence.at(-1);
    if (previous && previous.state === key) {
      previous.endMs = t(frame);
      previous.frames += 1;
    } else {
      sequence.push({ state: key, startMs: t(frame), endMs: t(frame), frames: 1 });
    }
  }
  const frameMs = frames.length > 1 ? round((frames.at(-1).t - frames[0].t) / (frames.length - 1)) : null;
  emitStep("states: layout states from the conversation page on, ms from the answer being stored", {
    frameMs,
    states: sequence.map((entry, index) => ({ ...entry, durationMs: index + 1 < sequence.length ? round(sequence[index + 1].startMs - entry.startMs) : null })),
  });

  // Reflows: frames where the conversation column or the panel changed its box.
  let reflows = 0;
  const reflowFrames = [];
  const afterStored = frames.filter((frame) => stored && frame.t >= stored.t);
  for (let index = 1; index < afterStored.length; index += 1) {
    const [a, b] = [afterStored[index - 1], afterStored[index]];
    const moved = (x, y) => (x === null) !== (y === null) || (x && y && (x.x !== y.x || x.w !== y.w));
    if (moved(a.col, b.col) || moved(a.panel, b.panel) || moved(a.card, b.card)) {
      reflows += 1;
      reflowFrames.push(round(b.t - stored.t));
    }
  }
  emitStep("reflows after the answer is stored", { frames: reflows, atMsFromStored: reflowFrames.slice(0, 30) });

  // Geometry: before the panel appears (last frame without it) and after (the final frame).
  const beforePanel = firstPanel ? frames[frames.indexOf(firstPanel) - 1] ?? null : null;
  const geometry = (frame) =>
    frame && {
      columns: frame.columns,
      col: frame.col && { x: frame.col.x, w: frame.col.w },
      panel: frame.panel && { x: frame.panel.x, y: frame.panel.y, w: frame.panel.w, h: frame.panel.h, skeleton: frame.panel.skeleton },
      panelInFirstScreen: frame.panel ? frame.panel.y >= frame.band.top - 1 && frame.panel.y < frame.band.bottom : null,
      panelShownPx: frame.panel ? Math.max(0, Math.min(frame.panel.y + frame.panel.h, frame.band.bottom) - Math.max(frame.panel.y, frame.band.top)) : null,
      card: frame.card && { x: frame.card.x, y: frame.card.y, w: frame.card.w, h: frame.card.h },
      chart: frame.chart && { w: frame.chart.w, h: frame.chart.h },
    };
  emitStep("geometry: before the panel appears, in its first frame, at the end", {
    before: geometry(beforePanel),
    firstPanelFrame: geometry(firstPanel),
    end: geometry(last),
  });

  // Positions of what the audience reads, before and after the panel appears.
  const positions = (frame) =>
    frame && {
      scrollY: frame.sy,
      band: frame.band,
      question: frame.q && { y: frame.q.y, h: frame.q.h, inBand: frame.q.inBand },
      paragraph1: frame.p[0] && { y: frame.p[0].y, h: frame.p[0].h, inBand: frame.p[0].inBand },
      paragraph2: frame.p[1] && { y: frame.p[1].y, h: frame.p[1].h, inBand: frame.p[1].inBand },
      paragraph3: frame.p[2] && { y: frame.p[2].y, h: frame.p[2].h, inBand: frame.p[2].inBand },
      cardHeader: frame.card?.h3 && { y: frame.card.h3.y, inBand: frame.card.h3.inBand },
      firstChart: frame.chart && { y: frame.chart.y, h: frame.chart.h, shown: frame.chart.shown },
    };
  // How far what the audience reads moved on the screen between two frames (viewport y; null when
  // it is not in one of them).
  const moved = (a, b) =>
    a && b
      ? {
          question: a.q && b.q ? round(b.q.y - a.q.y) : null,
          paragraph1: a.p[0] && b.p[0] ? round(b.p[0].y - a.p[0].y) : null,
          paragraph2: a.p[1] && b.p[1] ? round(b.p[1].y - a.p[1].y) : null,
          paragraph3: a.p[2] && b.p[2] ? round(b.p[2].y - a.p[2].y) : null,
          cardHeader: a.card?.h3 && b.card?.h3 ? round(b.card.h3.y - a.card.h3.y) : null,
          firstChart: a.chart && b.chart ? round(b.chart.y - a.chart.y) : null,
        }
      : null;
  const before = beforePanel;
  const lastStreaming = stored ? frames[frames.indexOf(stored) - 1] ?? null : null;
  emitStep("positions (viewport y, px) while streaming, when the answer is stored, before the panel, in its first frame, at the end", {
    lastStreamingFrame: positions(lastStreaming),
    storedFrame: positions(stored),
    beforePanel: positions(before),
    firstPanelFrame: positions(firstPanel),
    end: positions(last),
    movedAtStored: moved(lastStreaming, stored),
    movedAtPanel: moved(before, firstPanel),
    movedFromBeforePanelToEnd: moved(before, last),
    movedFromLastStreamingToEnd: moved(lastStreaming, last),
  });

  // Scrolling: the page's own scroll from the first conversation frame to the end.
  const conversationFrames = frames.filter((frame) => frame.path.startsWith("/conversations/"));
  const scrolls = [];
  for (let index = 1; index < conversationFrames.length; index += 1) {
    const [a, b] = [conversationFrames[index - 1], conversationFrames[index]];
    if (a.sy !== b.sy) scrolls.push({ atMsFromStored: round(b.t - (stored?.t ?? 0)), from: a.sy, to: b.sy });
  }
  const windowAfterPanel = firstPanel ? conversationFrames.filter((frame) => frame.t >= firstPanel.t - 100 && frame.t <= firstPanel.t + 600) : [];
  emitStep("scrolling", {
    scrollYAtHome: data.homeScrollY,
    scrollYAtFirstConversationFrame: conversationFrames[0]?.sy ?? null,
    scrollYBeforePanel: before?.sy ?? null,
    scrollYFirstPanelFrame: firstPanel?.sy ?? null,
    scrollYEnd: last.sy,
    scrollHeightBeforePanel: before?.sh ?? null,
    scrollHeightEnd: last.sh,
    pageScrolledByItselfBetweenSendAndEnd: scrolls.length > 0,
    scrollChanges: scrolls.slice(0, 40),
    scrollChangeCount: scrolls.length,
    scrollChangesAroundThePanel: scrolls.filter((entry) => windowAfterPanel.some((frame) => round(frame.t - (stored?.t ?? 0)) === entry.atMsFromStored)).length,
    paragraphReadBeforeSwitchWasLastParagraph: before?.p[1] ? "paragraph 2 (the last of the streaming answer)" : null,
  });

  // Redraw of the card's charts, and the requests around the switch.
  const switchT = firstPanel ? firstPanel.t : null;
  const canvasIds = (frame) => (frame ? frame.canvases.map((canvas) => canvas.id) : []);
  const idsBefore = canvasIds(before);
  const idsEnd = canvasIds(last);
  const everIds = [...new Set(conversationFrames.flatMap(canvasIds))];
  const chartSizes = [];
  for (const frame of conversationFrames) {
    const key = frame.canvases.map((canvas) => `${canvas.id}:${canvas.cw}`).join(",");
    if (chartSizes.at(-1)?.key !== key) chartSizes.push({ key, atMsFromStored: round(frame.t - (stored?.t ?? 0)) });
  }
  const runs = requests.filter((request) => request.method === "POST" && /\/dataset-runs$/.test(request.path));
  const requestLine = (request) => ({ atMsFromStored: round(request.t - (stored?.t ?? 0)), method: request.method, path: request.path.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, "<id>") });
  const loadingFrames = conversationFrames.filter((frame) => stored && frame.t >= stored.t && (frame.card === null || frame.canvases.length === 0 || frame.card.busy > 0 || frame.card.pulse > 0));
  emitStep("redraw", {
    chartCanvasIdsBeforePanel: idsBefore,
    chartCanvasIdsEnd: idsEnd,
    chartCanvasesEverSeen: everIds,
    canvasReplacedAtSwitch: idsBefore.length > 0 ? idsBefore.some((id) => !idsEnd.includes(id)) : null,
    chartWidthChanges: chartSizes,
    datasetRunRequestsTotal: runs.length,
    datasetRunRequestsBeforePanel: switchT === null ? null : runs.filter((request) => request.t < switchT).length,
    datasetRunRequestsAfterPanel: switchT === null ? null : runs.filter((request) => request.t >= switchT).length,
    apiRequests: requests.map(requestLine),
    framesWithoutCardChartOrWithLoadingAfterStored: loadingFrames.length,
    loadingFramesAtMsFromStored: loadingFrames.slice(0, 20).map((frame) => round(frame.t - (stored?.t ?? 0))),
  });

  // Focus, layout shifts, overflow, clipped text, errors.
  const shiftTotal = round(shifts.filter((entry) => !entry.hadRecentInput).reduce((sum, entry) => sum + entry.value, 0));
  const focusEvents = events.filter((event) => event.op === "focus");
  emitStep("focus and layout shifts", {
    activeElementBeforePanel: before?.focus ?? null,
    activeElementEnd: last.focus ?? null,
    focusEvents: focusEvents.map((event) => ({ atMsFromStored: round(event.t - (stored?.t ?? 0)), what: event.what })),
    layoutShiftTotal: shiftTotal,
    layoutShiftsUnsupported: data.shiftsUnsupported === true,
    layoutShifts: shifts.map((entry) => ({ atMsFromStored: round(entry.t - (stored?.t ?? 0)), value: entry.value, sources: entry.sources })),
  });
  emitStep("mutations of interest, ms from the answer being stored", {
    events: events
      .filter((event) => event.op !== "focus")
      .map((event) => ({ atMsFromStored: round(event.t - (stored?.t ?? 0)), ...event, t: undefined })),
  });
  if (KEEP_FRAMES) emitStep("frames", { frames });
}

async function walk(viewport, repeat, withShots) {
  const where = { viewport: viewport.id, repeat, latencyMs: LATENCY_MS, css: `${viewport.width}x${viewport.height}`, dsf: viewport.dsf, scheme: viewport.scheme };
  const context = await browser.newContext({
    locale: "zh-TW",
    timezoneId: "Asia/Taipei",
    viewport: { width: viewport.width, height: viewport.height },
    deviceScaleFactor: viewport.dsf,
    colorScheme: viewport.scheme,
  });
  await context.addInitScript(probeInstall);
  if (LATENCY_MS > 0) {
    // The settings the toolbar saves in sessionStorage (frontend/src/ui-preview/settings.ts).
    await context.addInitScript((slowMs) => {
      sessionStorage.setItem("trustiot-ui-preview", JSON.stringify({ role: "admin", platformAdmin: false, seed: "normal", slowMs }));
    }, LATENCY_MS);
  }
  const page = await context.newPage();
  const consoleErrors = [];
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text().slice(0, 300));
  });
  page.on("pageerror", (error) => consoleErrors.push(`pageerror: ${error.message.slice(0, 200)} @ ${(error.stack ?? "").split("\n").slice(1, 3).join(" ").slice(0, 200)}`));
  const frames = [];
  let cdp = null;
  try {
    await page.goto(`${BASE}/`);
    const box = page.getByRole("textbox", { name: "問 TrustIoT" });
    await box.waitFor({ state: "visible", timeout: WAIT_MS });
    await box.fill(REQUEST);
    const homeScrollY = await page.evaluate(() => Math.round(window.scrollY));
    await page.evaluate(([paragraphs, question]) => window.__probe.configure(paragraphs, question), [PARAGRAPHS, QUESTION_START]);
    if (withShots) {
      cdp = await context.newCDPSession(page);
      cdp.on("Page.screencastFrame", async ({ data, metadata, sessionId }) => {
        frames.push({ ts: metadata.timestamp, data });
        try {
          await cdp.send("Page.screencastFrameAck", { sessionId });
        } catch {
          // The page closed.
        }
      });
      await cdp.send("Page.startScreencast", { format: "jpeg", quality: 45, maxWidth: viewport.width, maxHeight: viewport.height, everyNthFrame: 1 });
    }
    await page.evaluate(() => window.__probe.start());
    await page.getByRole("button", { name: "送出" }).click();
    await page.evaluate(() => window.__probe.mark("send clicked"));
    // Conditions, in the order the audience sees them: the conversation page, the answer stored (its
    // last paragraph), the card and its charts, the panel, then a layout that stays as it is.
    const settled = page.waitForFunction(
      (frames) => {
        const probe = window.__probe;
        const last = probe.frames.at(-1);
        return (
          last !== undefined &&
          last.path.startsWith("/conversations/") &&
          last.p[2] !== null &&
          last.card !== null &&
          last.canvases.length > 0 &&
          last.panel !== null &&
          !last.panel.skeleton &&
          probe.unchangedFrames >= frames
        );
      },
      SETTLED_FRAMES,
      { timeout: WAIT_MS, polling: "raf" },
    );
    try {
      await settled;
    } catch {
      const state = await page.evaluate(() => {
        const last = window.__probe.frames.at(-1);
        return last ? { path: last.path, p3: last.p[2] !== null, card: last.card !== null, canvases: last.canvases.length, panel: last.panel !== null, unchanged: window.__probe.unchangedFrames } : null;
      });
      throw new Error(`timed out after ${WAIT_MS / 1000} s waiting for the stored answer, its card and charts, the draft panel and a layout unchanged for ${SETTLED_FRAMES} frames; last state ${JSON.stringify(state)}`);
    }
    const data = await page.evaluate(() => {
      const probe = window.__probe;
      probe.stop();
      return { frames: probe.frames, events: probe.events, shifts: probe.shifts, requests: probe.requests, t0: probe.t0, shiftsUnsupported: probe.shiftsUnsupported === true, timeOrigin: probe.timeOrigin, hidden: probe.hidden() };
    });
    data.homeScrollY = homeScrollY;
    emit(where, "setup", {
      lang: await page.evaluate(() => document.documentElement.lang),
      theme: await page.evaluate(() => document.documentElement.dataset.theme ?? null),
      hiddenPreviewOnly: data.hidden,
      request: REQUEST,
      framesSampled: data.frames.length,
      scrollYAtHomeBeforeSending: homeScrollY,
    });
    analyse(where, data);
    // The end state: overflow, clipped text, the panel's place, errors.
    const end = await page.evaluate(() => {
      const root = document.getElementById("root");
      const scroller = document.scrollingElement;
      const vw = document.documentElement.clientWidth;
      const leaving = [...root.querySelectorAll("*")].filter((element) => {
        const style = getComputedStyle(element);
        if (style.display === "none" || style.visibility === "hidden" || element.closest(".sr-only")) return false;
        const rect = element.getBoundingClientRect();
        if (rect.width <= 1 && rect.height <= 1) return false;
        if (rect.right <= vw + 0.5 && rect.left >= -0.5) return false;
        for (let up = element.parentElement; up && up.id !== "root"; up = up.parentElement) {
          if (getComputedStyle(up).overflowX !== "visible") {
            const upRect = up.getBoundingClientRect();
            if (upRect.right <= vw + 0.5 && upRect.left >= -0.5) return false;
            break;
          }
        }
        return true;
      });
      const clipped = [...root.querySelectorAll("*")].filter((element) => {
        const style = getComputedStyle(element);
        if (style.display === "none" || style.visibility === "hidden" || element.closest(".sr-only")) return false;
        const ownText = [...element.childNodes].some((node) => node.nodeType === 3 && node.textContent.trim() !== "");
        if (!ownText) return false;
        const clamps = style.webkitLineClamp !== "none" && style.webkitLineClamp !== "";
        const clips = style.textOverflow === "ellipsis" || clamps || ["hidden", "clip"].includes(style.overflowX);
        const spills = style.overflowX === "visible" && element.scrollWidth > element.clientWidth + 1 && style.display !== "inline";
        return (clips && (element.scrollWidth > element.clientWidth + 1 || (clamps && element.scrollHeight > element.clientHeight + 1))) || spills;
      });
      const text = (element) => (element.textContent ?? "").replace(/\s+/g, " ").trim().slice(0, 60);
      return {
        horizontalScroll: scroller.scrollWidth > vw || root.scrollWidth > root.clientWidth,
        leavingCount: leaving.length,
        leaving: leaving.slice(0, 6).map((element) => `${element.tagName.toLowerCase()} ${text(element)}`),
        clippedOrSpillingCount: clipped.length,
        clippedOrSpilling: clipped.slice(0, 8).map((element) => `${element.tagName.toLowerCase()} ${text(element)} (${element.clientWidth}/${element.scrollWidth})`),
      };
    });
    emit(where, "end state: overflow and clipped text", end);
    emit(where, "console errors", { count: consoleErrors.length, errors: consoleErrors.slice(0, 10) });

    // The card drawn first, then the panel: in production the card's version and the drafts list are two
    // requests that return in either order, and the walkthrough above has them return together (the
    // fake answers at once, or after the same delay). When the card comes first it is drawn at the
    // one-column width and laid out again when the panel appears. The same re-layout of an already drawn
    // card, without the drafts list in it, is the control viewport widened across the `xl` breakpoint:
    // the page that turns two columns has the card, its charts and the panel drawn, and the
    // columns switch as the panel's arrival switches them. It is an emulation of the cause, not of
    // the order of two requests, and is recorded as such.
    if (viewport.id === "1279x720-control" && repeat === 1 && LATENCY_MS === 0) {
      await page.evaluate(() => window.__probe.start());
      await page.setViewportSize({ width: 1280, height: 720 });
      await page.waitForFunction(
        (needed) => {
          const probe = window.__probe;
          const last = probe.frames.at(-1);
          return last !== undefined && last.columns > 1 && last.canvases.length > 0 && probe.unchangedFrames >= needed;
        },
        SETTLED_FRAMES,
        { timeout: WAIT_MS, polling: "raf" },
      );
      const widened = await page.evaluate(() => {
        const probe = window.__probe;
        probe.stop();
        return { frames: probe.frames, requests: probe.requests, shifts: probe.shifts, t0: probe.t0 };
      });
      const [first, final] = [widened.frames[0], widened.frames.at(-1)];
      const switched = widened.frames.find((frame) => frame.columns > 1);
      const ids = (frame) => frame.canvases.map((canvas) => canvas.id);
      const everIds = [...new Set(widened.frames.flatMap(ids))];
      const sizes = [];
      for (const frame of widened.frames) {
        const key = frame.canvases.map((canvas) => `${canvas.id}:${canvas.cw}`).join(",");
        if (sizes.at(-1)?.key !== key) sizes.push({ key, atMs: round(frame.t - widened.frames[0].t) });
      }
      const place = (frame, name) => frame?.[name] && { y: frame[name].y, h: frame[name].h, w: frame[name].w, inBand: frame[name].inBand };
      emit(where, "emulation: the drawn card re-laid when the page turns two columns (viewport 1279 to 1280)", {
        framesSampled: widened.frames.length,
        columnsBefore: first.columns,
        columnsAfter: final.columns,
        switchAtMs: switched ? round(switched.t - first.t) : null,
        columnWidth: { before: first.col?.w ?? null, after: final.col?.w ?? null },
        cardWidth: { before: first.card?.w ?? null, after: final.card?.w ?? null },
        chart: { before: place(first, "chart"), after: place(final, "chart") },
        canvasIdsBefore: ids(first),
        canvasIdsAfter: ids(final),
        canvasesEverSeen: everIds,
        canvasReplaced: ids(first).some((id) => !ids(final).includes(id)),
        chartWidthChanges: sizes,
        datasetRunRequestsDuring: widened.requests.filter((request) => request.method === "POST" && /\/dataset-runs$/.test(request.path)).length,
        apiRequestsDuring: widened.requests.length,
        framesLoadingOrWithoutChart: widened.frames.filter((frame) => frame.card === null || frame.canvases.length === 0 || frame.card.busy > 0 || frame.card.pulse > 0).length,
        movedPx: {
          question: first.q && final.q ? round(final.q.y - first.q.y) : null,
          paragraph2: first.p[1] && final.p[1] ? round(final.p[1].y - first.p[1].y) : null,
          paragraph3: first.p[2] && final.p[2] ? round(final.p[2].y - first.p[2].y) : null,
          cardHeader: first.card?.h3 && final.card?.h3 ? round(final.card.h3.y - first.card.h3.y) : null,
        },
        scrollY: { before: first.sy, after: final.sy },
        layoutShiftTotal: round(widened.shifts.filter((entry) => !entry.hadRecentInput).reduce((sum, entry) => sum + entry.value, 0)),
        layoutShiftsUnsupported: false,
      });
    }

    if (withShots && cdp) {
      await cdp.send("Page.stopScreencast");
      const switchEpoch = (() => {
        const first = data.frames.find((frame) => frame.panel !== null);
        return first ? (data.timeOrigin + first.t) / 1000 : null;
      })();
      const storedEpoch = (() => {
        const first = data.frames.find((frame) => frame.p[2] !== null);
        return first ? (data.timeOrigin + first.t) / 1000 : null;
      })();
      if (switchEpoch !== null && storedEpoch !== null) {
        const around = frames.filter((frame) => frame.ts >= storedEpoch - 0.35 && frame.ts <= switchEpoch + 0.45);
        const picked = around.length <= 8 ? around : around.filter((_, index) => index % Math.ceil(around.length / 8) === 0);
        for (const frame of picked) shots.push({ name: `${viewport.id}--${TREE}--L${LATENCY_MS}--${Math.round((frame.ts - storedEpoch) * 1000)}ms-from-stored`, base64: frame.data });
        if (frames.at(-1)) shots.push({ name: `${viewport.id}--${TREE}--L${LATENCY_MS}--final`, base64: frames.at(-1).data });
        emit(where, "screencast", { framesCaptured: frames.length, framesNearTheSwitch: around.length, printed: picked.length + 1, panelAtMsFromStored: Math.round((switchEpoch - storedEpoch) * 1000) });
      }
    }
  } catch (error) {
    failures.push(`${viewport.id} #${repeat}: ${error.message.split("\n")[0]}`);
    emit(where, "FAILED", { error: error.message.split("\n")[0], url: new URL(page.url()).pathname });
  }
  await context.close();
}

for (const viewport of VIEWPORTS) {
  for (let repeat = 1; repeat <= REPEATS; repeat += 1) {
    await walk(viewport, repeat, TAKE_SHOTS && repeat === REPEATS);
  }
}
await browser.close();
emit({}, "summary", { viewports: VIEWPORTS.map((viewport) => `${viewport.id} ${viewport.width}x${viewport.height}@${viewport.dsf} ${viewport.scheme}`), repeats: REPEATS, failures });

if (shots.length > 0) {
  console.log("=== SCREENSHOTS BEGIN ===");
  for (const shot of shots) console.log(`SHOT ${shot.name} ${shot.base64}`);
  console.log("=== SCREENSHOTS END ===");
}
// A run that did not finish makes the script fail, after the record and the screenshots are out.
process.exitCode = failures.length > 0 ? 1 : 0;
