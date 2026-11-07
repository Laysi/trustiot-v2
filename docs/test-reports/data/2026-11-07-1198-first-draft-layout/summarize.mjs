// Turns the JSON lines of browser-check.mjs (one file per tree) into the Markdown tables of the
// report. Usage: bun docs/test-reports/data/2026-11-07-1198-first-draft-layout/summarize.mjs
// <jsonl file>... (a file holds every line of one or more runs; the `tree` field names the tree).
import { readFileSync } from "node:fs";

const lines = process.argv
  .slice(2)
  .flatMap((file) => readFileSync(file, "utf8").split("\n"))
  .filter((line) => line.startsWith("{"))
  .map((line) => JSON.parse(line))
  // One tree at one request latency is one group: `B@0`, `B@2000`.
  .map((line) => (line.tree && line.latencyMs !== undefined ? { ...line, tree: `${line.tree}@${line.latencyMs}` } : line));

const trees = [...new Set(lines.map((line) => line.tree))].filter(Boolean).sort();
const viewports = [...new Set(lines.map((line) => line.viewport))].filter(Boolean);
const find = (tree, viewport, repeat, prefix) => lines.find((line) => line.tree === tree && line.viewport === viewport && line.repeat === repeat && line.step.startsWith(prefix));
const repeatsOf = (tree, viewport) => [...new Set(lines.filter((line) => line.tree === tree && line.viewport === viewport).map((line) => line.repeat))].sort();

/** The value of each repeat of one run, joined: "a / b". */
const perRepeat = (tree, viewport, prefix, pick) =>
  repeatsOf(tree, viewport)
    .map((repeat) => {
      const line = find(tree, viewport, repeat, prefix);
      if (!line) return "n/a";
      const value = pick(line);
      return value === null || value === undefined ? "–" : String(value);
    })
    .join(" · ");

const rows = (title, header, build) => {
  console.log(`\n#### ${title}\n`);
  console.log(`| ${header.join(" | ")} |`);
  console.log(`| ${header.map(() => "---").join(" | ")} |`);
  for (const tree of trees) {
    for (const viewport of viewports) {
      if (repeatsOf(tree, viewport).length === 0) continue;
      console.log(`| ${[tree, viewport, ...build(tree, viewport)].join(" | ")} |`);
    }
  }
};

const pf = (tree, viewport, prefix, pick) => perRepeat(tree, viewport, prefix, pick);

rows(
  "Timing: first appearance in ms from the answer being stored (repeats separated by ·)",
  ["tree", "viewport", "paragraph 1", "paragraph 2", "card", "chart", "panel", "two columns", "layout final", "reflow frames"],
  (tree, viewport) => [
    pf(tree, viewport, "timeline", (l) => l.sentAnd.paragraph1),
    pf(tree, viewport, "timeline", (l) => l.sentAnd.paragraph2),
    pf(tree, viewport, "timeline", (l) => l.sentAnd.card),
    pf(tree, viewport, "timeline", (l) => l.sentAnd.chart),
    pf(tree, viewport, "timeline", (l) => l.sentAnd.panel),
    pf(tree, viewport, "timeline", (l) => l.sentAnd.twoColumns),
    pf(tree, viewport, "timeline", (l) => l.sentAnd.layoutFinal),
    pf(tree, viewport, "reflows", (l) => l.frames),
  ],
);

console.log("\n#### Layout states (first repeat; `state` @ start ms, duration ms)\n");
for (const tree of trees) {
  for (const viewport of viewports) {
    const line = find(tree, viewport, repeatsOf(tree, viewport)[0], "states");
    if (!line) continue;
    console.log(`- ${tree} ${viewport} (frame ${line.frameMs} ms): ${line.states.map((state) => `\`${state.state}\` @${state.startMs} (${state.durationMs ?? "to end"})`).join(" → ")}`);
  }
}

rows(
  "Geometry: conversation column x/width and the draft panel before and after (px)",
  ["tree", "viewport", "column before", "column end", "panel first frame (x, y, w)", "panel end (x, y, w)", "panel starts in first screen", "panel px shown"],
  (tree, viewport) => {
    const g = (pick) => pf(tree, viewport, "geometry", pick);
    const c = (value) => (value ? `${value.x}/${value.w}` : "–");
    const p = (value) => (value ? `${value.x}, ${value.y}, ${value.w}` : "–");
    return [
      g((l) => c(l.before?.col)),
      g((l) => c(l.end.col)),
      g((l) => p(l.firstPanelFrame?.panel)),
      g((l) => p(l.end.panel)),
      g((l) => l.end.panelInFirstScreen),
      g((l) => l.end.panelShownPx),
    ];
  },
);

rows(
  "Shift of what the audience reads, viewport y in px (positive = down); at the answer being stored, at the panel, whole",
  ["tree", "viewport", "stored: question", "stored: paragraph 2", "panel: question", "panel: paragraph 2", "panel: paragraph 3", "panel: card header", "panel: chart", "last streaming → end: paragraph 2", "last streaming → end: question"],
  (tree, viewport) => {
    const m = (key, field) => pf(tree, viewport, "positions", (l) => l[key]?.[field]);
    return [
      m("movedAtStored", "question"),
      m("movedAtStored", "paragraph2"),
      m("movedAtPanel", "question"),
      m("movedAtPanel", "paragraph2"),
      m("movedAtPanel", "paragraph3"),
      m("movedAtPanel", "cardHeader"),
      m("movedAtPanel", "firstChart"),
      m("movedFromLastStreamingToEnd", "paragraph2"),
      m("movedFromLastStreamingToEnd", "question"),
    ];
  },
);

rows(
  "Scrolling (px): at home, first conversation frame, before the panel, at its first frame, end; the page scrolled by itself",
  ["tree", "viewport", "home", "first frame", "before panel", "panel frame", "end", "scroll changes", "paragraph 2 in band before", "paragraph 2 in band end", "paragraph 3 in band end"],
  (tree, viewport) => {
    const s = (pick) => pf(tree, viewport, "scrolling", pick);
    const inBand = (key, which) => pf(tree, viewport, "positions", (l) => l[which]?.[key]?.inBand);
    return [
      s((l) => l.scrollYAtHome),
      s((l) => l.scrollYAtFirstConversationFrame),
      s((l) => l.scrollYBeforePanel),
      s((l) => l.scrollYFirstPanelFrame),
      s((l) => l.scrollYEnd),
      s((l) => l.scrollChangeCount),
      inBand("paragraph2", "beforePanel"),
      inBand("paragraph2", "end"),
      inBand("paragraph3", "end"),
    ];
  },
);

rows(
  "Redraw, focus, layout shift, overflow, errors",
  ["tree", "viewport", "chart canvas replaced at switch", "dataset runs before / after the panel", "frames loading or without chart after stored", "chart width changes", "layout shift total", "active element before / end", "horizontal scroll", "clipped or spilling", "console errors"],
  (tree, viewport) => [
    pf(tree, viewport, "redraw", (l) => l.canvasReplacedAtSwitch),
    pf(tree, viewport, "redraw", (l) => `${l.datasetRunRequestsBeforePanel} / ${l.datasetRunRequestsAfterPanel}`),
    pf(tree, viewport, "redraw", (l) => l.framesWithoutCardChartOrWithLoadingAfterStored),
    pf(tree, viewport, "redraw", (l) => l.chartWidthChanges.map((change) => change.key).join(" > ")),
    pf(tree, viewport, "focus", (l) => l.layoutShiftTotal),
    pf(tree, viewport, "focus", (l) => `${l.activeElementBeforePanel?.what ?? "body"} / ${l.activeElementEnd?.what ?? "body"}`),
    pf(tree, viewport, "end state", (l) => l.horizontalScroll),
    pf(tree, viewport, "end state", (l) => l.clippedOrSpillingCount),
    pf(tree, viewport, "console errors", (l) => l.count),
  ],
);

console.log("\n#### Emulation: the drawn card re-laid when the viewport widens from 1279 to 1280 px\n");
for (const line of lines.filter((entry) => entry.step.startsWith("emulation"))) {
  const { tree, columnWidth, cardWidth, chart, canvasReplaced, datasetRunRequestsDuring, apiRequestsDuring, framesLoadingOrWithoutChart, movedPx, scrollY, layoutShiftTotal, chartWidthChanges } = line;
  console.log(
    `- ${tree}: column ${columnWidth.before} to ${columnWidth.after} px, card ${cardWidth.before} to ${cardWidth.after} px, chart ${chart.before?.w} to ${chart.after?.w} px wide; canvas replaced: ${canvasReplaced}; dataset runs during: ${datasetRunRequestsDuring}; API requests during: ${apiRequestsDuring}; frames loading or without chart: ${framesLoadingOrWithoutChart}; moved px ${JSON.stringify(movedPx)}; scrollY ${scrollY.before} to ${scrollY.after}; layout shift ${layoutShiftTotal}; canvas widths over time ${chartWidthChanges.map((change) => `${change.key}@${change.atMs}`).join(" > ")}`,
  );
}
