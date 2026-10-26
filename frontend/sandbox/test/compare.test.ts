// The comparison's rules one by one (src/compare.ts; README 「檢查規則」), on documents built in
// happy-dom. Computed styles and animations, which happy-dom does not have, come from stand-ins.
import { afterEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { Window } from "happy-dom";

import {
  compareDocument,
  concealsText,
  literalOf,
  parseCssContent,
  verdictOf,
  withoutNumbers,
} from "../src/compare.ts";
import { captureDom, type DomAccess, type DomWindow } from "../src/dom.ts";
import { type CheckRequest, CheckRequestSchema, type CheckVerdict, MAX_PROBLEMS } from "../src/protocol.ts";
import { layoutDom } from "./harness.ts";

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
  const access: DomAccess = {
    ...dom,
    style: (element, pseudo, property) =>
      options.style?.(element, pseudo, property) ?? dom.style(element, pseudo, property),
    liveAnimations: () => options.animations ?? 0,
  };
  return verdictOf(compareDocument(access, model, "zh-TW"));
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

  test("concealsText flags clipping and a zero box only with layout, never a boxless but visible mark", () => {
    const element = { nodeType: 1 } as unknown as Element;
    const stub = (over: {
      style?: Record<string, string>;
      box?: { width: number; height: number };
      scroll?: { scrollWidth: number; clientWidth: number; scrollHeight: number; clientHeight: number };
    }): DomAccess =>
      ({
        firstChild: () => null,
        nextSibling: () => null,
        nodeType: () => 1,
        style: (_e: Element, _p: string | null, property: string) => over.style?.[property] ?? "",
        box: () => ({ left: 0, top: 0, width: over.box?.width ?? 20, height: over.box?.height ?? 16 }),
        scrollSize: () => over.scroll ?? { scrollWidth: 20, clientWidth: 20, scrollHeight: 16, clientHeight: 16 },
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
    // display: none is hidden; display: contents has no box but shows its text, so it is not flagged
    // even when the box reads empty.
    expect(concealsText(stub({ style: { display: "none" } }), element, true)).toBe(true);
    expect(concealsText(stub({ style: { display: "contents" }, box: { width: 0, height: 0 } }), element, true)).toBe(
      false,
    );
    // An overflow box whose content fits (no overflow) is not clipped, so it passes.
    expect(concealsText(stub({ style: { "overflow-x": "hidden" } }), element, true)).toBe(false);
    expect(concealsText(stub({}), element, true)).toBe(false);
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
    expect(problems(verdictOf(compareDocument(dom, model, "zh-TW")))).toEqual([
      { kind: "unsupported_content", content: "shadow_root" },
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
    expect(problems(verdictOf(compareDocument(dom, model, "zh-TW")))).toEqual([
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
