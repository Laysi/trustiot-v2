// The comparison's rules one by one (src/compare.ts; README 「檢查規則」), on documents built in
// happy-dom. Computed styles and animations, which happy-dom does not have, come from stand-ins.
import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { Window } from "happy-dom";

import {
  colorAlpha,
  compareDocument,
  concealsText,
  fragmentIds,
  type Linear,
  literalOf,
  MAX_SLICE_STRETCH,
  MAX_TEXT_STRETCH,
  MAX_TEXT_TURN_DEG,
  MIN_LETTER_SPACING_EM,
  MIN_TEXT_ALPHA,
  MIN_TEXT_SIZE_PX,
  misreads,
  parseCssContent,
  stretchOf,
  verdictOf,
  withoutNumbers,
} from "../src/compare.ts";
import { type Box, captureDom, type DomAccess, type DomWindow, type Matrix } from "../src/dom.ts";
import {
  type CheckRequest,
  CheckRequestSchema,
  type CheckVerdict,
  MAX_PROBLEMS,
  type Problem,
} from "../src/protocol.ts";
import { layoutDom, pseudoInherits } from "./harness.ts";

const request: CheckRequest = CheckRequestSchema.parse({
  ...JSON.parse(readFileSync(new URL("../fixtures/request.json", import.meta.url), "utf8")),
  page: "",
});
const model = { spec: request.spec, results: request.datasets, tz: request.tz };

type Style = (element: Element, pseudo: string | null, property: string) => string | undefined;

interface Options {
  /** Computed style values; `undefined` falls back to happy-dom's. */
  style?: Style;
  animations?: number;
  /**
   * Judge as a browser with layout would, with these boxes (`box`, by default every element 20 × 16)
   * and SVG screen matrices (`matrix`, by default none, so each element's computed transform counts).
   */
  layout?: {
    box?: (element: Element, root: boolean) => Box | undefined;
    matrix?: (element: Element) => Matrix | null;
    /** `checkVisibility()`; true by default. */
    visible?: (element: Element) => boolean;
    /** The fragments of a text node; none by default. */
    textBoxes?: (node: Node) => Box[] | null;
    scroll?: (element: Element) => {
      scrollWidth: number;
      clientWidth: number;
      scrollHeight: number;
      clientHeight: number;
    };
  };
}

const windows: Window[] = [];

afterEach(async () => {
  for (const win of windows.splice(0)) await win.happyDOM.close();
});

function documentOf(body: string): { win: DomWindow; dom: DomAccess } {
  const happy = new Window({ url: "https://sandbox.example.test/sandbox/v1/sandbox.html" });
  windows.push(happy);
  happy.document.body.innerHTML = body;
  const win = happy as unknown as DomWindow;
  return { win, dom: layoutDom(captureDom(win)) };
}

function judge(body: string, options: Options = {}): CheckVerdict {
  const { dom } = documentOf(body);
  const layout = options.layout;
  // A pseudo-element inherits what the stand-in gives its element, as in a browser.
  const style = (element: Element, pseudo: string | null, property: string): string =>
    options.style?.(element, pseudo, property) ??
    (pseudo !== null && pseudoInherits(property)
      ? style(element, null, property)
      : dom.style(element, pseudo, property));
  const access: DomAccess = {
    ...dom,
    style,
    liveAnimations: () => options.animations ?? 0,
    ...(layout === undefined
      ? {}
      : {
          box: (element: Element) =>
            layout.box?.(element, element === dom.documentElement()) ?? { left: 0, top: 0, width: 20, height: 16 },
          scrollSize: (element: Element) =>
            layout.scroll?.(element) ?? { scrollWidth: 20, clientWidth: 20, scrollHeight: 16, clientHeight: 16 },
          screenMatrix: (element: Element) => layout.matrix?.(element) ?? null,
          visible: (element: Element) => layout.visible?.(element) ?? true,
          textBoxes: (node: Node) => (layout.textBoxes === undefined ? [] : layout.textBoxes(node)),
        }),
  };
  return verdictOf(compareDocument(access, model, "zh-TW", layout !== undefined));
}

const problems = (verdict: CheckVerdict) => (verdict.verdict === "failed" ? verdict.problems : []);
const marks = (verdict: CheckVerdict) => (verdict.verdict === "passed" ? verdict.report.marks : null);

describe("text marks", () => {
  test("text equal to tq.format of the value passes and reports the value", () => {
    const verdict = judge(
      '<p data-tq="f|week_kwh">690</p><p data-tq="c|daily_energy|kwh|0" data-tq-format=".1f">120.5</p>' +
        '<p data-tq="k|line_name">Line 2</p><p data-tq="c|daily_energy|kwh|3">\u2014</p>' +
        '<p data-tq="c|daily_energy|day|0" data-tq-format="%b %-d">10月 20</p><p data-tq="f|week_kwh">690</p>',
    );
    expect(marks(verdict)).toEqual([
      { reference: "f|week_kwh", value: 690 },
      { reference: "c|daily_energy|kwh|0", value: 120.5 },
      { reference: "k|line_name", value: "Line 2" },
      { reference: "c|daily_energy|kwh|3", value: null },
      { reference: "c|daily_energy|day|0", value: "2026-10-20" },
    ]);
  });

  test("other text, or a refused format, is a mismatch with the data's value", () => {
    expect(problems(judge('<p data-tq="f|week_kwh">691</p>'))).toEqual([
      { kind: "mismatch", reference: "f|week_kwh", value: 690, tag: "p" },
    ]);
    expect(problems(judge('<span data-tq="f|week_kwh" data-tq-format="5>4">5690</span>'))).toEqual([
      { kind: "mismatch", reference: "f|week_kwh", value: 690, tag: "span" },
    ]);
    // The whole text counts, child elements included.
    expect(problems(judge('<p data-tq="f|week_kwh">690<b>0</b></p>'))).toEqual([
      { kind: "mismatch", reference: "f|week_kwh", value: 690, tag: "p" },
    ]);
  });

  test("a mark whose rendered text may differ from its textContent is a mismatch", () => {
    // Right textContent, but a bidi override, a hidden or transparent mark, or a restyled first letter.
    const style =
      (property: string, value: string, pseudo: string | null = null): Style =>
      (_e, p, prop) =>
        p === pseudo && prop === property ? value : undefined;
    const bidi = style("unicode-bidi", "bidi-override");
    const hidden = style("visibility", "hidden");
    const gone = style("display", "none");
    const transparent = style("color", "transparent");
    const firstLetterSize: Style = (_e, pseudo, property) =>
      property !== "font-size" ? undefined : pseudo === "::first-letter" ? "0px" : "16px";
    const firstLetterColor = style("color", "rgba(0, 0, 0, 0)", "::first-letter");
    expect(problems(judge('<b data-tq="f|week_kwh">690</b>690', { style: bidi }))[0]).toEqual({
      kind: "mismatch",
      reference: "f|week_kwh",
      value: 690,
      tag: "b",
    });
    for (const s of [hidden, gone, transparent, firstLetterSize, firstLetterColor]) {
      expect(problems(judge('<b data-tq="f|week_kwh">690</b>', { style: s }))).toEqual([
        { kind: "mismatch", reference: "f|week_kwh", value: 690, tag: "b" },
      ]);
    }
    // An element child alone, with matching text, is flagged by structure (no computed style needed).
    expect(problems(judge('<b data-tq="f|week_kwh">690<i></i></b>'))).toEqual([
      { kind: "mismatch", reference: "f|week_kwh", value: 690, tag: "b" },
    ]);
  });

  test("concealsText flags clipping, a zero box and skipped content only with layout", () => {
    const element = { nodeType: 1 } as unknown as Element;
    const stub = (over: {
      style?: Record<string, string>;
      box?: { width: number; height: number };
      scroll?: { scrollWidth: number; clientWidth: number; scrollHeight: number; clientHeight: number };
      visible?: boolean;
    }): DomAccess =>
      ({
        documentElement: () => null,
        firstChild: () => null,
        nextSibling: () => null,
        parentNode: () => null,
        nodeType: () => 1,
        localName: () => "b",
        namespaceURI: () => "http://www.w3.org/1999/xhtml",
        getAttribute: () => null,
        textContent: () => "",
        screenMatrix: () => null,
        style: (_e: Element, pseudo: string | null, property: string) =>
          (pseudo === null ? over.style?.[property] : undefined) ?? "",
        box: () => ({ left: 0, top: 0, width: over.box?.width ?? 20, height: over.box?.height ?? 16 }),
        scrollSize: () => over.scroll ?? { scrollWidth: 20, clientWidth: 20, scrollHeight: 16, clientHeight: 16 },
        visible: () => over.visible ?? true,
        textBoxes: () => [],
      }) as unknown as DomAccess;
    const clipped = stub({
      style: { "overflow-x": "hidden" },
      scroll: { scrollWidth: 80, clientWidth: 20, scrollHeight: 16, clientHeight: 16 },
    });
    // With layout, an overflowing clipping box and a zero-size box (font-size: 0) are flagged;
    // without layout (a unit DOM reports every box empty), neither is, so plain marks pass.
    expect(concealsText(clipped, element, true)).toBe(true);
    expect(concealsText(clipped, element, false)).toBe(false);
    expect(concealsText(stub({ box: { width: 0, height: 16 } }), element, true)).toBe(true);
    expect(concealsText(stub({ box: { width: 0, height: 16 } }), element, false)).toBe(false);
    // display: none is hidden; display: contents has no box of its own to isolate its text from the
    // bidi order around it, so it is flagged too, with layout or without.
    expect(concealsText(stub({ style: { display: "none" } }), element, true)).toBe(true);
    expect(concealsText(stub({ style: { display: "contents" } }), element, false)).toBe(true);
    // Content the browser skips (a closed <details>, content-visibility: hidden) keeps its box.
    expect(concealsText(stub({ visible: false }), element, true)).toBe(true);
    expect(concealsText(stub({ visible: false }), element, false)).toBe(false);
    // An overflow box whose content fits (no overflow) is not clipped, so it passes.
    expect(concealsText(stub({ style: { "overflow-x": "hidden" } }), element, true)).toBe(false);
    expect(concealsText(stub({}), element, true)).toBe(false);
  });
});

describe("layout is the caller's fact, not the page's", () => {
  const page =
    '<p data-tq="f|week_kwh">690</p><svg>' +
    '<rect data-tq="c|daily_energy|kwh|0" data-tq-chart="bars" data-tq-channel="height" height="10"/></svg>';
  const emptyRoot =
    (width: number, height: number) =>
    (_element: Element, root: boolean): Box | undefined =>
      root ? { left: 0, top: 0, width, height } : undefined;

  test("with layout, plain marks in a root with an area pass", () => {
    expect(marks(judge(page, { layout: {} }))).toEqual([
      { reference: "f|week_kwh", value: 690 },
      { reference: "c|daily_energy|kwh|0", value: 120.5 },
    ]);
  });

  test("with layout, a root box without an area makes every mark a finding", () => {
    for (const [width, height] of [
      [0, 0],
      [1200, 0],
      [0, 400],
    ] as const) {
      expect(problems(judge(page, { layout: { box: emptyRoot(width, height) } }))).toEqual([
        { kind: "mismatch", reference: "f|week_kwh", value: 690, tag: "p" },
        { kind: "mark_geometry", reference: "c|daily_energy|kwh|0", channel: "height" },
      ]);
    }
  });

  test("without layout (a unit test's DOM) the root's empty box decides nothing", () => {
    expect(problems(judge(page))).toEqual([]);
  });
});

/** Computed styles of elements by id: `{ week: { color: "..." } }`; a pseudo-element's as `"week::first-letter"`. */
type Table = Record<string, Record<string, string>>;

const tables = (...list: Table[]) => list;

function styles(table: Table): Style {
  return (element, pseudo, property) => table[`${element.id}${pseudo ?? ""}`]?.[property];
}

const mismatch = (tag: "b" | "text" = "b"): Problem[] => [
  { kind: "mismatch", reference: "f|week_kwh", value: 690, tag },
];

describe("faint text", () => {
  test("colorAlpha reads the alpha of every colour syntax a browser serializes", () => {
    const cases: [string, number | null][] = [
      ["rgb(31, 41, 51)", 1],
      ["rgba(0, 0, 0, 0)", 0],
      ["rgba(0, 0, 0, 0.05)", 0.05],
      ["rgb(0 0 0 / 40%)", 0.4],
      ["hsl(120 50% 50% / 0.5)", 0.5],
      ["hwb(120 10% 10% / 0)", 0],
      ["lab(50 20 30 / 0)", 0],
      ["lch(50 20 30)", 1],
      ["oklab(0.5 0.1 0.1 / 0.25)", 0.25],
      ["oklch(0.5 0.1 120 / none)", 0],
      ["color(display-p3 1 0 0 / 0.02)", 0.02],
      ["color(srgb 1 0 0)", 1],
      ["COLOR(SRGB 1 0 0 / 1E-3)", 0.001],
      ["#000", 1],
      ["#0008", 0x88 / 255],
      ["#00000000", 0],
      ["transparent", 0],
      ["none", 0],
      ["context-fill", 0],
      // Fail closed: what the comparison cannot read paints nothing.
      ["rgb(0 0 0 / calc(1))", 0],
      ["rgba(0, 0, 0, 0.5, 1)", 0],
      ['url("#gradient")', 0],
      ["color-mix(in srgb, red, blue)", 0],
      // A unit DOM leaves keywords as written: nothing to read.
      ["red", null],
      ["currentColor", null],
      ["", null],
    ];
    for (const [value, alpha] of cases) {
      expect([value, colorAlpha(value)]).toEqual([value, alpha]);
    }
  });

  test("a mark painted fainter than MIN_TEXT_ALPHA, through its colours, opacity and ancestors, is a mismatch", () => {
    expect(MIN_TEXT_ALPHA).toBe(0.1);
    const page = '<div id="card"><p id="line">本週 <b id="week" data-tq="f|week_kwh">690</b></p></div>';
    for (const table of tables(
      { week: { color: "lab(50 20 30 / 0)" } },
      { week: { color: "oklch(0.5 0.1 120 / 0.05)" } },
      { week: { color: "color(display-p3 0 0 0 / 0.02)" } },
      { week: { "-webkit-text-fill-color": "rgba(0, 0, 0, 0)" } },
      // 0.3 × 0.3 = 0.09: each alone is visible, together they are not.
      { card: { opacity: "0.3" }, week: { opacity: "0.3" } },
      { line: { filter: "blur(0px) opacity(0.05)" } },
      { "week::first-letter": { opacity: "0.05" } },
      { "week::first-line": { "-webkit-text-fill-color": "rgb(0 0 0 / 5%)" } },
      { "week::selection": { color: "rgba(0, 0, 0, 0)" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch()]);
    }
    for (const table of tables(
      { week: { color: "oklch(0.5 0.1 120 / 0.2)" } },
      { week: { color: "rgb(0 0 0 / 40%)" } },
      { card: { opacity: "0.5" } },
      { line: { filter: "opacity(0.5)" } },
      { "week::selection": { color: "rgb(255, 255, 255)", "-webkit-text-fill-color": "rgb(255, 255, 255)" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, []]);
    }
  });

  test("SVG text is painted with its fill and fill-opacity", () => {
    const page = '<svg><text id="week" data-tq="f|week_kwh">690</text></svg>';
    for (const table of tables(
      { week: { fill: "rgb(0, 0, 0)", "fill-opacity": "0.05" } },
      { week: { fill: "none" } },
      { week: { fill: 'url("#gradient")' } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch("text")]);
    }
    expect(problems(judge(page, { style: styles({ week: { fill: "rgb(0, 0, 0)", "fill-opacity": "0.5" } }) }))).toEqual(
      [],
    );
  });
});

describe("turned, mirrored and stretched text", () => {
  const turn = (degrees: number, scale = 1): Linear => {
    const radians = (degrees * Math.PI) / 180;
    return [
      scale * Math.cos(radians),
      scale * Math.sin(radians),
      -scale * Math.sin(radians),
      scale * Math.cos(radians),
    ];
  };

  test("misreads allows translation, uniform scaling and a slant up to MAX_TEXT_TURN_DEG, nothing else", () => {
    expect(MAX_TEXT_TURN_DEG).toBe(45);
    expect(MAX_TEXT_STRETCH).toBe(4);
    for (const allowed of [turn(0), turn(0, 3), turn(0, 0.5), turn(-45), turn(45), turn(-30, 2)]) {
      expect([allowed, misreads(allowed)]).toEqual([allowed, false]);
    }
    // skewX(45deg) stretches by 2.6, under the limit.
    expect(misreads([1, 0, 1, 1])).toBe(false);
    for (const refused of [
      turn(46),
      turn(-90),
      turn(180),
      [-1, 0, 0, 1] as Linear,
      [1, 0, 0, -1] as Linear,
      [5, 0, 0, 1] as Linear,
      [0, 0, 0, 1] as Linear,
      [Number.NaN, 0, 0, 1] as Linear,
      null,
    ]) {
      expect([refused, misreads(refused)]).toEqual([refused, true]);
    }
    expect(stretchOf(turn(30, 2))).toBeCloseTo(1, 9);
    expect(stretchOf([3, 0, 0, 1])).toBeCloseTo(3, 9);
  });

  test("a transform of the mark or an ancestor that mirrors, turns or stretches it is a mismatch", () => {
    const page = '<div id="card"><p id="line">本週 <b id="week" data-tq="f|week_kwh">690</b></p></div>';
    for (const table of tables(
      { week: { transform: "matrix(-1, 0, 0, 1, 0, 0)" } },
      { card: { rotate: "180deg" } },
      { line: { rotate: "x 180deg" } },
      { week: { rotate: "-90deg" } },
      { week: { rotate: "0.5turn" } },
      { week: { scale: "-1 1" } },
      { week: { transform: "matrix(5, 0, 0, 1, 0, 0)" } },
      // Each 60° about y alone is a stretch of 2; composed in 3D they turn the text over (120°).
      {
        week: {
          rotate: "y 60deg",
          transform: "matrix3d(0.5, 0, -0.866025, 0, 0, 1, 0, 0, 0.866025, 0, 0.5, 0, 0, 0, 0, 1)",
        },
      },
      // rotateY(180deg)
      { line: { transform: "matrix3d(-1, 0, 0, 0, 0, 1, 0, 0, 0, 0, -1, 0, 0, 0, 0, 1)" } },
      // Perspective, a negative w, preserve-3d: no flat picture describes them.
      { week: { transform: "matrix3d(1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, -0.01, 0, 0, 0, 1)" } },
      { week: { transform: "matrix3d(1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, -1)" } },
      { card: { perspective: "100px" } },
      { card: { "transform-style": "preserve-3d" } },
      // Unreadable (a browser computes a matrix): fail closed.
      { week: { transform: "rotate(180deg)" } },
      { "week::first-letter": { transform: "matrix(-1, 0, 0, 1, 0, 0)" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch()]);
    }
    for (const table of tables(
      { week: { transform: "matrix(0.707107, -0.707107, 0.707107, 0.707107, 0, 0)" } },
      { card: { transform: "matrix(2, 0, 0, 2, 40, 10)" } },
      { week: { rotate: "1 1 0 45deg" } },
      { line: { transform: "matrix3d(1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1)" } },
      { card: { perspective: "none", "transform-style": "flat" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, []]);
    }
  });

  test("text drawn smaller than MIN_TEXT_SIZE_PX, by its font size, a scale or zoom, is a mismatch", () => {
    expect(MIN_TEXT_SIZE_PX).toBe(2);
    const page = '<div id="card"><b id="week" data-tq="f|week_kwh">690</b></div>';
    for (const table of tables(
      { week: { "font-size": "1px" } },
      { card: { transform: "matrix(0.01, 0, 0, 0.01, 0, 0)" } },
      { card: { zoom: "0.1" } },
      { card: { zoom: "0" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch()]);
    }
    for (const table of tables({ card: { zoom: "0.5" } }, { card: { transform: "matrix(0.25, 0, 0, 0.25, 0, 0)" } })) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, []]);
    }
  });

  test("with layout, an SVG mark's screen matrix decides, ancestors and viewBox included", () => {
    const page = '<svg><g><text id="week" data-tq="f|week_kwh">690</text></g></svg>';
    const at =
      (m: Linear) =>
      (element: Element): Matrix | null =>
        element.id === "week" ? { a: m[0], b: m[1], c: m[2], d: m[3], e: 10, f: 20 } : null;
    expect(problems(judge(page, { layout: { matrix: at([-1, 0, 0, 1]) } }))).toEqual(mismatch("text"));
    expect(problems(judge(page, { layout: { matrix: at(turn(180, 2)) } }))).toEqual(mismatch("text"));
    expect(problems(judge(page, { layout: { matrix: at(turn(-45, 1.5)) } }))).toEqual([]);
    // A viewBox shrinking 16px text to 0.8px.
    expect(problems(judge(page, { layout: { matrix: at([0.05, 0, 0, 0.05]) } }))).toEqual(mismatch("text"));
    // Without layout there is no screen matrix: the computed transforms count.
    expect(problems(judge(page))).toEqual([]);
  });

  test("SVG text that places its glyphs one by one is a mismatch", () => {
    for (const page of [
      '<svg><text data-tq="f|week_kwh" x="30 20 10">690</text></svg>',
      '<svg><text data-tq="f|week_kwh" x="0" dx="0,-12,-12">690</text></svg>',
      '<svg><text data-tq="f|week_kwh" rotate="180">690</text></svg>',
      '<svg><text data-tq="f|week_kwh" textLength="2">690</text></svg>',
      '<svg><text x="10 20 30"><tspan data-tq="f|week_kwh">690</tspan></text></svg>',
      '<svg><text><textPath href="#p" data-tq="f|week_kwh">690</textPath></text></svg>',
    ]) {
      const tag = page.includes("<tspan") ? "tspan" : page.includes("<textPath") ? "textPath" : "text";
      expect([page, problems(judge(page))]).toEqual([
        page,
        [{ kind: "mismatch", reference: "f|week_kwh", value: 690, tag }],
      ]);
    }
    // d3's tick labels: one x, y and dy each.
    expect(problems(judge('<svg><text data-tq="f|week_kwh" x="0" y="9" dy="0.71em">690</text></svg>'))).toEqual([]);
  });

  test("stroked SVG text is a mismatch unless the stroke is read as absent; a paint that cannot be read is drawn", () => {
    const page = '<svg><text id="week" data-tq="f|week_kwh">690</text></svg>';
    for (const table of tables(
      { week: { stroke: "rgb(0, 0, 0)", "stroke-width": "6px" } },
      { week: { stroke: "rgba(0, 0, 0, 0.5)", "stroke-width": "1px", "stroke-opacity": "0.5" } },
      // A gradient or pattern, a paint with a fallback colour, another function, an unreadable alpha.
      { week: { stroke: 'url("#g")', "stroke-width": "6px" } },
      { week: { stroke: 'url("#g") rgb(0, 0, 0)', "stroke-width": "6px" } },
      { week: { stroke: "linear-gradient(red, blue)", "stroke-width": "6px" } },
      { week: { stroke: "rgb(0 0 0 / calc(1))", "stroke-width": "6px" } },
      // A keyword a browser would have resolved, a width or opacity that cannot be read.
      { week: { stroke: "red", "stroke-width": "6px" } },
      { week: { stroke: "rgb(0, 0, 0)", "stroke-width": "thick" } },
      { week: { stroke: "rgb(0, 0, 0)", "stroke-width": "6px", "stroke-opacity": "var(--a)" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch("text")]);
    }
    for (const table of tables(
      { week: { stroke: "none", "stroke-width": "1px" } },
      { week: { stroke: "transparent", "stroke-width": "6px" } },
      { week: { stroke: "rgba(0, 0, 0, 0)", "stroke-width": "6px" } },
      { week: { stroke: "rgb(0 0 0 / 0)", "stroke-width": "6px" } },
      { week: { stroke: 'url("#g")', "stroke-width": "0px" } },
      { week: { stroke: "rgb(0, 0, 0)", "stroke-width": "6px", "stroke-opacity": "0" } },
      // A DOM without computed styles.
      { week: { stroke: "", "stroke-width": "6px" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, []]);
    }
  });

  test("letter spacing tighter than MIN_LETTER_SPACING_EM is a mismatch", () => {
    expect(MIN_LETTER_SPACING_EM).toBe(-0.2);
    const page = '<b id="week" data-tq="f|week_kwh">690</b>';
    const spaced = (spacing: string, pseudo = "") =>
      styles({ week: { "font-size": "16px" }, [`week${pseudo}`]: { "font-size": "16px", "letter-spacing": spacing } });
    expect(problems(judge(page, { style: spaced("-4px") }))).toEqual(mismatch());
    expect(problems(judge(page, { style: spaced("-8px", "::first-letter") }))).toEqual(mismatch());
    expect(problems(judge(page, { style: spaced("-0.5em") }))).toEqual(mismatch());
    expect(problems(judge(page, { style: spaced("-2px") }))).toEqual([]);
    expect(problems(judge(page, { style: spaced("normal") }))).toEqual([]);
    expect(problems(judge(page, { style: spaced("3px") }))).toEqual([]);
  });
});

describe("the computed styles a text mark keeps plain", () => {
  const page = '<div id="card"><p id="line">本週 <b id="week" data-tq="f|week_kwh">690</b></p></div>';

  test("a value outside the allowlist, on the mark or an ancestor, is a mismatch", () => {
    for (const [property, value] of [
      ["writing-mode", "vertical-rl"],
      ["writing-mode", "sideways-lr"],
      ["direction", "rtl"],
      ["text-orientation", "upright"],
      ["unicode-bidi", "bidi-override"],
      ["unicode-bidi", "plaintext"],
      ["-webkit-rtl-ordering", "visual"],
      ["offset-path", "ray(180deg)"],
      ["contain", "paint"],
      ["contain", "content"],
      ["contain", "strict"],
      ["contain", "layout paint"],
      ["content-visibility", "hidden"],
      ["content-visibility", "auto"],
      ["clip-path", "inset(0px 50%)"],
      ["mask-image", "linear-gradient(rgb(0, 0, 0), rgba(0, 0, 0, 0))"],
      ["-webkit-mask-box-image-source", "linear-gradient(rgb(0, 0, 0), rgba(0, 0, 0, 0))"],
      ["clip", "rect(0px, 4px, 4px, 0px)"],
      ["border-shape", "circle(10px)"],
      ["-webkit-line-clamp", "1"],
      ["text-fit", "grow"],
      ["position-visibility", "no-overflow"],
      ["position-anchor", "--hidden"],
      ["position-area", "bottom"],
      ["content", "linear-gradient(rgb(255, 255, 255), rgb(255, 255, 255))"],
      ["-webkit-box-reflect", "right"],
      ["filter", "blur(4px)"],
      ["filter", "drop-shadow(rgba(0, 0, 0, 0.5) 0px 1px 2px)"],
      ["filter", "opacity(0.8) blur(1px)"],
      ["filter", 'url("#copy")'],
      ["mix-blend-mode", "difference"],
      // An ancestor's decorations are drawn through the mark's text.
      ["text-decoration-line", "line-through"],
      ["text-decoration-line", "underline"],
      ["outline-offset", "-20px"],
      ["-webkit-user-modify", "read-write"],
    ] as const) {
      for (const id of ["week", "card"]) {
        const table = { [id]: { [property]: value } };
        expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch()]);
      }
    }
  });

  test("the plain values pass, on the mark and its ancestors", () => {
    for (const table of tables(
      {
        card: {
          "writing-mode": "horizontal-tb",
          direction: "ltr",
          "text-orientation": "mixed",
          "unicode-bidi": "normal",
        },
        line: { "unicode-bidi": "embed", "-webkit-rtl-ordering": "logical", "offset-path": "none" },
      },
      { card: { contain: "layout style" }, line: { contain: "size" } },
      { card: { "content-visibility": "visible", "clip-path": "none", "mask-image": "none", clip: "auto" } },
      { card: { filter: "opacity(0.8)" }, line: { filter: "none" } },
      { card: { "outline-offset": "2px", "mix-blend-mode": "normal", "-webkit-user-modify": "read-only" } },
      { card: { "position-visibility": "anchors-visible", "position-anchor": "normal", "position-area": "none" } },
      { card: { "position-visibility": "always", "position-anchor": "none", content: "normal" } },
      {
        week: {
          "unicode-bidi": "isolate",
          "-webkit-text-security": "none",
          "text-combine-upright": "none",
          "text-shadow": "none",
          "-webkit-text-stroke-width": "0px",
          "font-size-adjust": "none",
          "text-size-adjust": "100%",
          "text-emphasis-style": "none",
          "-webkit-text-decorations-in-effect": "none",
          "text-decoration-line": "none",
          "initial-letter": "normal",
          // A paragraph's indent, inherited by an inline mark.
          "text-indent": "32px",
          display: "inline",
        },
      },
      { week: { "text-indent": "5% hanging", display: "inline" } },
      // An inline mark has no first line of its own to indent; neither has SVG text.
      { week: { "text-indent": "9999px", display: "inline" } },
      { week: { "text-indent": "0px", display: "block" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, []]);
    }
  });

  test("a mark that is a block container keeps its own first line unindented", () => {
    for (const display of ["block", "inline-block", "table-cell", "list-item", "flow-root", "inline-flex"]) {
      for (const indent of ["9999px", "2em", "1px", "5%", "0px hanging"]) {
        const table = { week: { display, "text-indent": indent } };
        expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch()]);
      }
    }
    // SVG text has no indent, whatever it inherits.
    const svg = '<svg><text id="t" data-tq="f|week_kwh">690</text></svg>';
    const table = { t: { display: "block", "text-indent": "9999px" } };
    expect([table, problems(judge(svg, { style: styles(table) }))]).toEqual([table, []]);
  });

  test("an anchor counts only for an absolutely positioned box; auto, an earlier initial value, passes elsewhere", () => {
    // `auto` is the initial position-anchor in browsers that follow an earlier draft, and anchor
    // positioning moves and hides only absolutely positioned boxes.
    for (const table of tables(
      { week: { "position-anchor": "auto" }, card: { "position-anchor": "auto", position: "relative" } },
      { line: { "position-anchor": "auto", position: "sticky" } },
      {
        card: { "position-anchor": "none", position: "absolute" },
        week: { "position-anchor": "normal", position: "fixed" },
      },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, []]);
    }
    for (const table of tables(
      { week: { "position-anchor": "auto", position: "absolute" } },
      { card: { "position-anchor": "auto", position: "fixed" } },
      { line: { "position-anchor": "--hidden", position: "static" } },
      { week: { "position-anchor": "--hidden", position: "absolute" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch()]);
    }
  });

  test("the mark's own glyphs: masked, combined, shadowed, stroked, resized, emphasised, struck or moved out", () => {
    for (const [property, value] of [
      // The sandbox's styles isolate every mark; a page that undoes it lets neighbouring text reorder it.
      ["unicode-bidi", "normal"],
      ["unicode-bidi", "embed"],
      ["-webkit-text-security", "disc"],
      ["text-combine-upright", "all"],
      ["text-shadow", "rgb(0, 0, 0) -10px 0px 0px"],
      ["-webkit-text-stroke-width", "6px"],
      ["font-size-adjust", "0.05"],
      ["text-size-adjust", "300%"],
      ["text-emphasis-style", '"8"'],
      ["-webkit-text-decorations-in-effect", "line-through"],
      ["text-decoration-line", "line-through"],
      ["initial-letter", "3"],
      ["text-indent", "-9999px"],
      ["text-indent", "-50%"],
      // Unreadable: fail closed.
      ["text-indent", "calc(10px - 20%)"],
    ] as const) {
      const table = { week: { [property]: value } };
      const found: Problem[] = problems(judge(page, { style: styles(table) })).filter(
        (problem) => problem.kind === "mismatch",
      );
      expect([table, found]).toEqual([table, mismatch()]);
    }
    // On an ancestor they are what the mark inherits (or, for decorations, what is drawn through it),
    // which a browser reports on the mark itself.
    for (const table of tables({ card: { "text-shadow": "rgb(0, 0, 0) -10px 0px 0px", "text-indent": "-9999px" } })) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, []]);
    }
  });

  test("a restyled ::first-letter or ::first-line of the mark or an ancestor, or a ::selection that hides or copies, is a mismatch", () => {
    for (const table of tables(
      { "card::first-letter": { float: "right" } },
      { "line::first-letter": { "margin-left": "-20px" } },
      { "line::first-line": { "font-size": "1px" } },
      { "card::first-line": { color: "rgb(255, 0, 0)" } },
      { "week::first-letter": { "vertical-align": "super" } },
      { "week::first-letter": { "padding-right": "40px" } },
      { "week::first-letter": { "-webkit-text-security": "disc" } },
      { "week::first-line": { "text-decoration-line": "line-through" } },
      { "week::first-line": { "text-shadow": "rgb(0, 0, 0) 10px 0px 0px" } },
      { "week::selection": { "text-shadow": "rgb(0, 0, 0) 10px 0px 0px" } },
      { "week::selection": { "-webkit-text-stroke-width": "4px" } },
      { "week::selection": { "text-decoration-line": "line-through" } },
      { "week::selection": { "-webkit-text-fill-color": "rgba(0, 0, 0, 0)" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, mismatch()]);
    }
    // What a pseudo-element inherits from its element unchanged, and the element's own values of the
    // properties a pseudo-element does not inherit, are no restyling.
    for (const table of tables(
      { card: { color: "rgb(31, 41, 51)", "font-size": "18px", "letter-spacing": "1px" } },
      { card: { opacity: "0.5", "margin-left": "8px", float: "left", transform: "matrix(1, 0, 0, 1, 4, 0)" } },
      { week: { "vertical-align": "super", "padding-right": "4px" } },
    )) {
      expect([table, problems(judge(page, { style: styles(table) }))]).toEqual([table, []]);
    }
  });

  test("a <details> draws its content in ::details-content, which no element's style shows", () => {
    const details =
      '<details open id="d"><summary>產線 <b data-tq="k|line_name">Line 2</b></summary>' +
      '<p><b id="week" data-tq="f|week_kwh">690</b></p></details>';
    for (const table of tables(
      { "d::details-content": { rotate: "180deg" } },
      { "d::details-content": { transform: "matrix(-1, 0, 0, 1, 0, 0)" } },
      { "d::details-content": { zoom: "0.05" } },
      { "d::details-content": { perspective: "100px" } },
      { "d::details-content": { opacity: "0.02" } },
      { "d::details-content": { filter: "blur(4px)" } },
      { "d::details-content": { "clip-path": "inset(0px 100% 0px 0px)" } },
      { "d::details-content": { "content-visibility": "hidden" } },
      // Its box cannot be read, so it must not clip at all.
      { "d::details-content": { "overflow-y": "hidden" } },
    )) {
      // The summary is drawn outside it: only the mark in the content is a finding.
      expect([table, problems(judge(details, { style: styles(table) }))]).toEqual([table, mismatch()]);
    }
    const plain = {
      "d::details-content": { rotate: "none", opacity: "1", "overflow-x": "visible", "content-visibility": "visible" },
    };
    expect(problems(judge(details, { style: styles(plain) }))).toEqual([]);
    // A <details> as a text mark draws its own text there.
    expect(problems(judge('<details open data-tq="f|week_kwh">690</details>'))).toEqual([
      { kind: "mismatch", reference: "f|week_kwh", value: 690, tag: "details" },
    ]);
  });

  test("bidi controls in a mark's text reorder it", () => {
    const { dom } = documentOf('<b id="a">\u2067690\u2069</b><b id="b">\u200f690</b><b id="c">690</b>');
    const root = dom.documentElement() as Element;
    const byId = (id: string) =>
      (root as unknown as { ownerDocument: Document }).ownerDocument.getElementById(id) as Element;
    expect(concealsText(dom, byId("a"), false)).toBe(true);
    expect(concealsText(dom, byId("b"), false)).toBe(true);
    expect(concealsText(dom, byId("c"), false)).toBe(false);
  });

  test("with layout, a mark outside an ancestor's clipping padding box, skipped, or broken over lines is a mismatch", () => {
    const at =
      (boxes: Record<string, Box>) =>
      (element: Element): Box | undefined =>
        boxes[element.id];
    const card = { left: 0, top: 0, width: 100, height: 40 };
    const judgeAt = (style: Table, week: Box, more: Options["layout"] = {}) =>
      problems(judge(page, { style: styles(style), layout: { box: at({ card, week }), ...more } }));
    const inside = { left: 10, top: 10, width: 30, height: 16 };
    const right = { left: 200, top: 10, width: 30, height: 16 };
    const below = { left: 10, top: 100, width: 30, height: 16 };
    expect(judgeAt({ card: { "overflow-x": "hidden" } }, right)).toEqual(mismatch());
    expect(judgeAt({ card: { "overflow-x": "clip", "overflow-y": "clip" } }, below)).toEqual(mismatch());
    expect(judgeAt({ card: { "overflow-x": "hidden" } }, inside)).toEqual([]);
    // One axis clips only along it.
    expect(judgeAt({ card: { "overflow-y": "clip" } }, right)).toEqual([]);
    expect(judgeAt({ card: { "overflow-y": "clip" } }, below)).toEqual(mismatch());
    // The clip is the padding box: a mark drawn over the border is cut off. A scaled box's borders
    // are as wide as it draws them.
    const bordered = { "overflow-x": "hidden", "border-left-width": "20px", "border-right-width": "20px" };
    const past = { left: 22, top: 10, width: 30, height: 16 };
    expect(judgeAt({ card: bordered }, { left: 2, top: 10, width: 14, height: 16 })).toEqual(mismatch());
    expect(judgeAt({ card: bordered }, past)).toEqual([]);
    expect(judgeAt({ card: { ...bordered, scale: "2" } }, past)).toEqual(mismatch());
    // A turned clipping box clips to less than its bounding box, and an unreadable border hides
    // where it clips: no rectangle reads the clip, so nothing under it passes.
    expect(judgeAt({ card: { "overflow-x": "hidden", rotate: "10deg" } }, inside)).toEqual(mismatch());
    expect(judgeAt({ card: { "overflow-x": "hidden", "border-left-width": "thick" } }, inside)).toEqual(mismatch());
    expect(judgeAt({ card: { rotate: "10deg" } }, inside)).toEqual([]);
    // A scrolling ancestor lets a reader scroll to the mark.
    expect(judgeAt({ card: { "overflow-x": "auto" } }, right)).toEqual([]);
    // The root's and <body>'s overflow is the viewport's, which a frame grows to fit.
    const bodyHides = (element: Element, pseudo: string | null, property: string) =>
      pseudo === null && element.localName === "body" && property.startsWith("overflow-") ? "hidden" : undefined;
    expect(problems(judge(page, { style: bodyHides, layout: { box: at({ week: right }) } }))).toEqual([]);
    // Content the browser skips keeps its box.
    expect(judgeAt({}, inside, { visible: (element) => element.id !== "week" })).toEqual(mismatch());
    // A number broken over lines, or its fragments out of order, reads as other numbers.
    const fragments = (...boxes: Box[]) => ({ textBoxes: () => boxes });
    const first = { left: 10, top: 10, width: 10, height: 16 };
    expect(judgeAt({}, inside, fragments(first, { left: 10, top: 30, width: 20, height: 16 }))).toEqual(mismatch());
    expect(judgeAt({}, inside, fragments(first, { left: 2, top: 10, width: 6, height: 16 }))).toEqual(mismatch());
    expect(judgeAt({}, inside, fragments(first, { left: 20, top: 10, width: 20, height: 16 }))).toEqual([]);
    expect(judgeAt({}, inside, fragments(first))).toEqual([]);
    // A text in more fragments than the DOM reads is broken.
    expect(judgeAt({}, inside, { textBoxes: () => null })).toEqual(mismatch());
  });
});

describe("marks and copies outside the document's own drawing", () => {
  test("a mark inside an SVG element that is never drawn where it stands is a finding", () => {
    for (const container of ["defs", "marker", "pattern", "clipPath", "mask", "symbol"]) {
      expect([
        container,
        problems(judge(`<svg><${container}><text data-tq="f|week_kwh">690</text></${container}></svg>`)),
      ]).toEqual([container, mismatch("text")]);
      const bar = '<rect data-tq="c|daily_energy|kwh|0" data-tq-chart="bars" data-tq-channel="height" height="10"/>';
      expect([container, problems(judge(`<svg><${container}>${bar}</${container}></svg>`))]).toEqual([
        container,
        [{ kind: "mark_geometry", reference: "c|daily_energy|kwh|0", channel: "height" }],
      ]);
    }
    expect(problems(judge('<svg><g><text data-tq="f|week_kwh">690</text></g></svg>'))).toEqual([]);
  });

  test("a <use> that copies text or a mark is unmapped, wherever it sits", () => {
    const copy: Problem = { kind: "unmapped", tag: "use" };
    const text = '<svg><text id="t" data-tq="f|week_kwh">690</text>';
    expect(problems(judge(`${text}<g data-tq-decor=""><use href="#t"/></g></svg>`))).toEqual([copy]);
    expect(problems(judge(`${text}<g data-tq-decor=""><use xlink:href="#t"/></g></svg>`))).toEqual([copy]);
    // The fragment of a URL, percent-decoded.
    expect(problems(judge(`${text}<g data-tq-decor=""><use href="#%74"/></g></svg>`))).toEqual([copy]);
    // Any reference other than a bare fragment is refused, whatever it names: a URL of this
    // document, another document whose fragment happens to match a local id, or one that does not.
    for (const href of [
      " https://sandbox.example.test/x#t",
      "https://other.example/chart.svg#t",
      "chart.svg#shape",
      "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%3E%3Ctext%20id='u'%3E9%3C/text%3E%3C/svg%3E#u",
      "chart.svg",
    ]) {
      const body = `${text}<g data-tq-decor=""><use href="${href}"/></g></svg>`;
      expect([href, problems(judge(body))]).toEqual([href, [copy]]);
    }
    expect(problems(judge(`${text}<g data-tq-decor=""><use href="#t" xlink:href="chart.svg#t"/></g></svg>`))).toEqual([
      copy,
    ]);
    // A group that holds text, plain text, a foreignObject, a graphic mark, the whole <svg>.
    for (const body of [
      '<svg><g id="g"><text>Line</text></g><g data-tq-decor=""><use href="#g"/></g></svg>',
      '<svg><foreignObject id="f"><p>Line</p></foreignObject><g data-tq-decor=""><use href="#f"/></g></svg>',
      '<svg><rect id="r" data-tq="c|daily_energy|kwh|0" data-tq-chart="bars" data-tq-channel="height" height="10"/><g data-tq-decor=""><use href="#r"/></g></svg>',
      '<svg id="s"><text data-tq="f|week_kwh">690</text><g data-tq-decor=""><use href="#s"/></g></svg>',
    ]) {
      expect([body, problems(judge(body))]).toEqual([body, [copy]]);
    }
    // A use of a shape that holds no text, as decor, passes.
    expect(
      problems(judge('<svg><g data-tq-decor=""><defs><path id="p" d="M0 0L1 1"/></defs><use href="#p"/></g></svg>')),
    ).toEqual([]);
    // So does one with an empty reference, which draws nothing.
    expect(problems(judge('<svg><g data-tq-decor=""><use href=" "/></g></svg>'))).toEqual([]);
  });

  test("feImage draws a picture or a copy of an element: content the check cannot read", () => {
    expect(problems(judge('<svg><filter id="f"><feImage href="#t"/></filter></svg>'))).toEqual([
      { kind: "unsupported_content", content: "image" },
    ]);
  });

  test("fragmentIds reads a bare fragment as written and percent-decoded, and no other URL", () => {
    expect(fragmentIds("#week")).toEqual(["week"]);
    expect(fragmentIds(" \t#we\nek ")).toEqual(["week"]);
    expect(fragmentIds("#%77eek")).toEqual(["%77eek", "week"]);
    expect(fragmentIds("#%E0%A4%A")).toEqual(["%E0%A4%A"]);
    expect(fragmentIds(" \n")).toEqual([]);
    expect(fragmentIds("https://sandbox.example.test/page#week")).toBeNull();
    expect(fragmentIds("https://other.example/chart.svg#week")).toBeNull();
    expect(fragmentIds("week")).toBeNull();
  });
});

describe("references that do not map", () => {
  test("a malformed or undeclared reference names no reference", () => {
    expect(problems(judge('<p data-tq="f|Week">1</p>'))).toEqual([{ kind: "unmapped", tag: "p" }]);
    expect(problems(judge('<p data-tq="f|month_kwh">1</p>'))).toEqual([{ kind: "unmapped", tag: "p" }]);
    expect(problems(judge('<p data-tq="c|weekly|kwh|0">1</p>'))).toEqual([{ kind: "unmapped", tag: "p" }]);
  });

  test("a declared reference that does not resolve names it", () => {
    expect(problems(judge('<p data-tq="c|daily_energy|kwh|7">1</p>'))).toEqual([
      { kind: "unmapped", reference: "c|daily_energy|kwh|7", tag: "p" },
    ]);
  });

  test("a tick reference outside an axis, or on a graphic, does not map", () => {
    expect(problems(judge('<p data-tq="t|daily_energy|kwh|5000">5,000</p>'))).toEqual([
      { kind: "unmapped", reference: "t|daily_energy|kwh|5000", tag: "p" },
    ]);
    expect(
      problems(
        judge(
          '<svg><g data-tq-axis=""><rect data-tq="t|daily_energy|kwh|50" data-tq-chart="a" data-tq-channel="height" height="5"/></g></svg>',
        ),
      ),
    ).toEqual([{ kind: "unmapped", reference: "t|daily_energy|kwh|50", tag: "rect" }]);
  });

  test("a graphic mark needs a valid chart name and channel", () => {
    for (const attributes of [
      'data-tq-chart="Bars" data-tq-channel="height"',
      'data-tq-chart="bars" data-tq-channel="depth"',
      'data-tq-channel="height"',
      'data-tq-chart="bars"',
    ]) {
      expect(problems(judge(`<svg><rect data-tq="f|week_kwh" ${attributes} height="10"/></svg>`))).toEqual([
        { kind: "unmapped", reference: "f|week_kwh", tag: "rect" },
      ]);
    }
  });

  test("every SVG shape belongs to a mark, an axis or decor", () => {
    expect(
      problems(
        judge(
          '<svg><rect/><circle/><use href="#x"/><g data-tq-decor=""><path d="M0,0L1,1"/></g><g data-tq-axis=""><line x2="5"/></g></svg>',
        ),
      ),
    ).toEqual([
      { kind: "unmapped", tag: "rect" },
      { kind: "unmapped", tag: "circle" },
      { kind: "unmapped", tag: "use" },
    ]);
  });
});

describe("numbers outside the marks", () => {
  test("text nodes, with full-width and other decimal digits", () => {
    expect(problems(judge("<p>Total 1,234.5 kWh</p>"))).toEqual([
      { kind: "literal_number", place: "text", literal: "1,234.5", tag: "p" },
    ]);
    expect(problems(judge("<h2>\uFF11\uFF12\uFF13 \u5EA6</h2>"))).toEqual([
      { kind: "literal_number", place: "text", literal: "\uFF11\uFF12\uFF13", tag: "h2" },
    ]);
    expect(problems(judge("<p>\u0661\u0662</p>"))).toEqual([
      { kind: "literal_number", place: "text", literal: "\u0661\u0662", tag: "p" },
    ]);
  });

  test("text inside a graphic mark counts; style and script text do not", () => {
    expect(
      problems(
        judge(
          '<style>.a { width: 10px }</style><svg><g data-tq="f|week_kwh" data-tq-chart="bars" data-tq-channel="height"><rect data-tq-decor="" height="5"/><text>42</text></g></svg>',
        ),
      ),
    ).toEqual([
      { kind: "literal_number", place: "text", literal: "42", tag: "text" },
      // The group has no size here, so its height does not show its value either.
      { kind: "mark_geometry", reference: "f|week_kwh", channel: "height" },
    ]);
  });

  test("attributes a browser shows or reads aloud", () => {
    expect(problems(judge('<div title="Peak 35.5 kW" aria-label="ok"></div>'))).toEqual([
      { kind: "literal_number", place: "title", literal: "35.5", tag: "div" },
    ]);
    expect(problems(judge('<div role="meter" aria-valuenow="7"></div><img alt="x">'))).toEqual([
      { kind: "literal_number", place: "aria-valuenow", literal: "7", tag: "div" },
      { kind: "unsupported_content", content: "image" },
    ]);
    // Other attributes are not text.
    expect(problems(judge('<div data-x="42" class="c42"></div>'))).toEqual([]);
  });

  test("a literal is at most 32 characters", () => {
    const long = "9".repeat(40);
    expect(problems(judge(`<p>${long}</p>`))).toEqual([
      { kind: "literal_number", place: "text", literal: "9".repeat(32), tag: "p" },
    ]);
  });
});

describe("generated content", () => {
  const pseudo =
    (values: Record<string, string>): Style =>
    (element, which, property) =>
      element.id === "x" ? values[`${which ?? ""}${property}`] : undefined;

  test("a number in ::before, ::after or ::marker content", () => {
    expect(problems(judge('<p id="x">a</p>', { style: pseudo({ "::beforecontent": '"42 "' }) }))).toEqual([
      { kind: "literal_number", place: "css-content", literal: "42", tag: "p" },
    ]);
    expect(problems(judge('<p id="x">a</p>', { style: pseudo({ "::aftercontent": '"\\34 2"' }) }))).toEqual([
      { kind: "literal_number", place: "css-content", literal: "42", tag: "p" },
    ]);
  });

  test("quotes come from the quotes property", () => {
    const style = pseudo({ "::beforecontent": "open-quote", "::beforequotes": '"7" "8"' });
    expect(problems(judge('<p id="x">a</p>', { style }))).toEqual([
      { kind: "literal_number", place: "css-content", literal: "7", tag: "p" },
    ]);
  });

  test("custom highlights restyle text no element's style shows", () => {
    const { dom } = documentOf('<p data-tq="f|week_kwh">690</p>');
    const comparison = compareDocument({ ...dom, highlights: () => 1 }, model, "zh-TW", false);
    expect(problems(verdictOf(comparison))).toEqual([{ kind: "unsupported_content", content: "generated_content" }]);
    expect(problems(verdictOf(compareDocument(dom, model, "zh-TW", false)))).toEqual([]);
  });

  test("a scroll container's scroll buttons and scroll markers", () => {
    const page = '<div id="s">尖峰</div>';
    const button = { "s::scroll-button(right)": { content: '"777"' } };
    expect(problems(judge(page, { style: styles({ s: { "overflow-x": "auto" }, ...button }) }))).toEqual([
      { kind: "literal_number", place: "css-content", literal: "777", tag: "div" },
    ]);
    // Not a scroll container: it draws no scroll buttons.
    expect(problems(judge(page, { style: styles(button) }))).toEqual([]);
    expect(problems(judge(page, { style: styles({ s: { "scroll-marker-group": "after" } }) }))).toEqual([
      { kind: "unsupported_content", content: "generated_content" },
    ]);
    expect(problems(judge(page, { style: styles({ s: { "scroll-marker-group": "none" } }) }))).toEqual([]);
  });

  test("digits a hyphenate-character or text-emphasis-style draws, judged where the value is set", () => {
    const literal = (digits: string): Problem[] => [
      { kind: "literal_number", place: "css-content", literal: digits, tag: "p" },
    ];
    expect(problems(judge('<p id="p">尖峰</p>', { style: styles({ p: { "hyphenate-character": '"9"' } }) }))).toEqual(
      literal("9"),
    );
    expect(problems(judge('<p id="p">尖峰</p>', { style: styles({ p: { "text-emphasis-style": '"8"' } }) }))).toEqual(
      literal("8"),
    );
    // A child inherits the value: once, where it is set.
    const inherited: Style = (element, pseudo, property) =>
      pseudo === null && property === "hyphenate-character" && element.closest?.("#p") ? '"9"' : undefined;
    expect(problems(judge('<p id="p">尖峰<span>x</span></p>', { style: inherited }))).toEqual(literal("9"));
    expect(
      problems(
        judge('<p id="p">尖峰</p>', {
          style: styles({ p: { "hyphenate-character": "auto", "text-emphasis-style": "filled sesame" } }),
        }),
      ),
    ).toEqual([]);
  });

  test("counters, attributes and images in content are not supported", () => {
    for (const content of ["counter(item)", 'attr(title) "x"', 'url("x.png")', "counters(a, '.')"]) {
      expect(problems(judge('<p id="x">a</p>', { style: pseudo({ "::beforecontent": content }) }))).toEqual([
        { kind: "unsupported_content", content: "generated_content" },
      ]);
    }
    expect(problems(judge('<p id="x">a</p>', { style: pseudo({ "::beforecontent": "normal" }) }))).toEqual([]);
    expect(problems(judge('<p id="x">a</p>', { style: pseudo({ "::beforecontent": '"kWh"' }) }))).toEqual([]);
  });

  test("numbered list items show their ordinal; symbol markers pass", () => {
    const list =
      (type: string): Style =>
      (element, which, property) => {
        if (element.localName !== "li" || which !== null) return undefined;
        if (property === "display") return "list-item";
        if (property === "list-style-type") return type;
        return undefined;
      };
    expect(problems(judge("<ol><li>a</li><li>b</li></ol>", { style: list("decimal") }))).toEqual([
      { kind: "literal_number", place: "list-marker", literal: "1", tag: "li" },
      { kind: "literal_number", place: "list-marker", literal: "2", tag: "li" },
    ]);
    // Letters and roman numerals count too: outside its range a counter style writes decimal digits.
    expect(problems(judge("<ol><li>a</li></ol>", { style: list("lower-roman") }))).toEqual([
      { kind: "literal_number", place: "list-marker", literal: "1", tag: "li" },
    ]);
    expect(problems(judge("<ul><li>a</li></ul>", { style: list("disc") }))).toEqual([]);
    expect(problems(judge("<ul><li>a</li></ul>", { style: list("none") }))).toEqual([]);
    expect(problems(judge("<ul><li>a</li></ul>", { style: list('"3) "') }))).toEqual([
      { kind: "literal_number", place: "list-marker", literal: "3", tag: "li" },
    ]);
    expect(problems(judge("<ul><li>a</li></ul>", { style: list('"-"') }))).toEqual([]);
  });
});

describe("content the check cannot read, and animations", () => {
  test("pictures, media, frames and form fields", () => {
    expect(
      problems(
        judge('<canvas></canvas><video></video><iframe></iframe><object></object><input><svg><image href="x"/></svg>'),
      ),
    ).toEqual([
      { kind: "unsupported_content", content: "canvas" },
      { kind: "unsupported_content", content: "media" },
      { kind: "unsupported_content", content: "frame" },
      { kind: "unsupported_content", content: "object" },
      { kind: "unsupported_content", content: "form_field" },
      { kind: "unsupported_content", content: "image" },
    ]);
  });

  test("an open shadow root", () => {
    const { win, dom } = documentOf('<div id="host"></div>');
    win.document.getElementById("host")?.attachShadow({ mode: "open" });
    expect(problems(verdictOf(compareDocument(dom, model, "zh-TW", false)))).toEqual([
      { kind: "unsupported_content", content: "shadow_root" },
    ]);
  });

  test("a marquee moves its content unreported: a render timeout", () => {
    expect(problems(judge('<marquee><b data-tq="f|week_kwh">690</b></marquee>'))).toEqual([
      { kind: "timeout", phase: "render" },
    ]);
  });

  test("SVG animation elements and live animations are a render timeout", () => {
    expect(problems(judge('<svg><g data-tq-decor=""><rect/><animate attributeName="x"/></g></svg>'))).toEqual([
      { kind: "timeout", phase: "render" },
    ]);
    expect(problems(judge("<p>a</p>", { animations: 1 }))).toEqual([{ kind: "timeout", phase: "render" }]);
  });
});

describe("size limits", () => {
  test("more marks than the limit is judged by that alone", () => {
    const { win, dom } = documentOf("");
    const fragment = win.document.createDocumentFragment();
    for (let i = 0; i <= 10_000; i++) {
      const item = win.document.createElement("i");
      item.setAttribute("data-tq", "f|nothing");
      fragment.appendChild(item);
    }
    win.document.body.appendChild(fragment);
    expect(problems(verdictOf(compareDocument(dom, model, "zh-TW", false)))).toEqual([
      { kind: "too_large", limit: "marks", max: 10_000 },
    ]);
  });
});

describe("geometry", () => {
  const bars = (heights: number[], row = (i: number) => i) =>
    `<svg>${heights
      .map(
        (height, i) =>
          `<rect data-tq="c|daily_energy|kwh|${row(i)}" data-tq-chart="bars" data-tq-channel="height" height="${height}"/>`,
      )
      .join("")}</svg>`;

  test("bar heights proportional to the values pass; one off fails; null is zero", () => {
    // kwh: 120.5, 98.25, 131, null, 87.5
    expect(problems(judge(bars([241, 196.5, 262, 0, 175])))).toEqual([]);
    expect(problems(judge(bars([241, 196.5, 300, 0, 175])))).toEqual([
      { kind: "mark_geometry", reference: "c|daily_energy|kwh|2", channel: "height" },
    ]);
    expect(problems(judge(bars([241, 196.5, 262, 5, 175])))).toEqual([
      { kind: "mark_geometry", reference: "c|daily_energy|kwh|3", channel: "height" },
    ]);
  });

  test("a mark whose value is not a number or a time fails its channel", () => {
    expect(
      problems(
        judge('<svg><rect data-tq="k|line_name" data-tq-chart="bars" data-tq-channel="width" width="5"/></svg>'),
      ),
    ).toEqual([{ kind: "mark_geometry", reference: "k|line_name", channel: "width" }]);
  });

  test("points on a line pass; one off it fails", () => {
    const points = (ys: number[]) =>
      `<svg>${ys
        .map(
          (y, i) =>
            `<circle data-tq="c|hourly_power|kw|${i}" data-tq-chart="line" data-tq-channel="y" cy="${y}" r="2"/>`,
        )
        .join("")}</svg>`;
    // kw: 12.5, 11.75, 11, 10.5: y = 400 - 10 * kw
    expect(problems(judge(points([275, 282.5, 290, 295])))).toEqual([]);
    expect(problems(judge(points([275, 282.5, 250, 295])))).toEqual([
      { kind: "mark_geometry", reference: "c|hourly_power|kw|2", channel: "y" },
    ]);
  });

  test("an angle needs a d3.arc path", () => {
    expect(
      problems(judge('<svg><rect data-tq="f|week_kwh" data-tq-chart="pie" data-tq-channel="angle" width="5"/></svg>')),
    ).toEqual([{ kind: "mark_geometry", reference: "f|week_kwh", channel: "angle" }]);
  });

  test("a pie scaled more in one direction than another (past MAX_SLICE_STRETCH) shows other angles", () => {
    // meter_usage: 310.5, 298.25, 615; d3.arc's slices, clockwise from 12 o'clock.
    const values = [310.5, 298.25, 615];
    const total = values.reduce((sum, value) => sum + value, 0);
    let start = 0;
    const slices = values.map((value, i) => {
      const end = start + (2 * Math.PI * value) / total;
      const point = (angle: number) => `${100 * Math.sin(angle)},${-100 * Math.cos(angle)}`;
      const large = end - start > Math.PI ? 1 : 0;
      const d = `M${point(start)}A100,100,0,${large},1,${point(end)}L0,0Z`;
      start = end;
      return `<path data-tq="c|meter_usage|usage|${i}" data-tq-chart="share" data-tq-channel="angle" d="${d}"/>`;
    });
    const page = `<svg><g>${slices.join("")}</g></svg>`;
    const scaled =
      (a: number, d: number) =>
      (element: Element): Matrix | null =>
        element.localName === "path" ? { a, b: 0, c: 0, d, e: 120, f: 120 } : null;
    expect(MAX_SLICE_STRETCH).toBe(1.01);
    expect(problems(judge(page, { layout: { matrix: scaled(1.5, 1.5) } }))).toEqual([]);
    // Mirrored or turned, a pie still shows its angles.
    expect(problems(judge(page, { layout: { matrix: scaled(-1, 1) } }))).toEqual([]);
    expect(problems(judge(page, { layout: { matrix: scaled(2, 1) } }))).toEqual(
      [0, 1, 2].map((i) => ({ kind: "mark_geometry", reference: `c|meter_usage|usage|${i}`, channel: "angle" })),
    );
  });

  test("axis ticks inside the column's range and on a line pass", () => {
    const axis = (values: number[], at: (i: number) => number) =>
      `<svg><g data-tq-axis="">${values
        .map(
          (value, i) =>
            `<g transform="translate(0,${at(i)})"><line x2="-6"/><text data-tq="t|daily_energy|kwh|${value}">${value.toLocaleString("en")}</text></g>`,
        )
        .join("")}</g></svg>`;
    expect(problems(judge(axis([0, 50, 100, 150], (i) => 400 - 100 * i)))).toEqual([]);
    expect(problems(judge(axis([0, 50, 100, 5000], (i) => 400 - 100 * i)))).toEqual([
      { kind: "mark_geometry", reference: "t|daily_energy|kwh|5000", channel: "axis" },
    ]);
    expect(problems(judge(axis([0, 50, 100, 150], (i) => (i === 2 ? 50 : 400 - 100 * i))))).toEqual([
      { kind: "mark_geometry", reference: "t|daily_energy|kwh|100", channel: "axis" },
    ]);
  });
});

describe("the verdict", () => {
  test("identical problems count once, and at most 20 are kept", () => {
    const many = Array.from({ length: 30 }, (_, i) => `<p>${i}</p>`).join("");
    const verdict = judge(`<p>1</p>${many}`);
    expect(problems(verdict).length).toBe(MAX_PROBLEMS);
    expect(problems(verdict)[0]).toEqual({ kind: "literal_number", place: "text", literal: "1", tag: "p" });
    expect(problems(verdict)[1]).toEqual({ kind: "literal_number", place: "text", literal: "0", tag: "p" });
  });

  test("a page without marks or numbers passes with no marks", () => {
    expect(judge("<p>Energy this week</p>")).toEqual({ verdict: "passed", report: { marks: [] } });
  });
});

describe("text helpers", () => {
  test("literalOf takes the first number with its separators", () => {
    expect(literalOf("from 1,234.5 to 6")).toBe("1,234.5");
    expect(literalOf("v2")).toBe("2");
    expect(literalOf("no digits, at all.")).toBeNull();
    expect(literalOf("\uFF19\uFF19")).toBe("\uFF19\uFF19");
  });

  test("withoutNumbers replaces every number with an em dash", () => {
    expect(withoutNumbers("1 and 2,000.5 kWh")).toBe("\u2014 and \u2014 kWh");
  });

  test("parseCssContent reads strings, escapes, quotes and anything else", () => {
    expect(parseCssContent('"a" "b\\"c"')).toEqual({ strings: ["a", 'b"c'], quotes: false, other: false });
    expect(parseCssContent('"\\31 2\\33"')).toEqual({ strings: ["123"], quotes: false, other: false });
    expect(parseCssContent('"\\0"').strings).toEqual(["\uFFFD"]);
    expect(parseCssContent("open-quote no-close-quote")).toEqual({ strings: [], quotes: true, other: false });
    expect(parseCssContent("none")).toEqual({ strings: [], quotes: false, other: false });
    expect(parseCssContent('"a" / "alt"').strings).toEqual(["a", "alt"]);
    expect(parseCssContent('counter(x) "a"').other).toBe(true);
    expect(parseCssContent("inherit").other).toBe(true);
    expect(parseCssContent("#").other).toBe(true);
  });
});
