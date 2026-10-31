// The comparison (frontend/sandbox/README.md, 「檢查規則」 and 「比對程式」): reads the rendered
// document and judges every rule against data the caller gives. One implementation with two
// entry points:
//
// - the checker's (`compare.js`, entries/compare.ts): run in an isolated world the page cannot
//   reach, it returns the contract's verdict (`checkVerdict`);
// - the display-time one (the runtime, view mode): run in the page's realm with DOM functions
//   captured before the page ran, it only decides what to show (`findings`, with a fix each).
//
// Everything here is synchronous: the page cannot run while the document is read.
import type { Box, DomAccess } from "./dom.ts";
import { FormatError, formatValue } from "./format.ts";
import {
  disproportionate,
  numberAxisRange,
  offLine,
  type Placed,
  RELATIVE_TOLERANCE,
  sliceAngle,
  timeAxisRange,
  within,
} from "./geometry.ts";
import {
  type CheckVerdict,
  ELEMENT_TAGS,
  formatReference,
  LITERAL_PLACES,
  type Locale,
  MARK_CHANNELS,
  MAX_ELEMENTS,
  MAX_LITERAL_CHARS,
  MAX_MARKS,
  MAX_PROBLEMS,
  type MarkChannel,
  NAME_PATTERN,
  type Problem,
  parseReference,
  type Reference,
} from "./protocol.ts";
import { parseWallTime, tickTimeDate } from "./time.ts";
import {
  type ContractValue,
  columnType,
  columnValues,
  declares,
  type Model,
  type Resolved,
  resolve,
  resultOf,
} from "./values.ts";

export const SVG_NS = "http://www.w3.org/2000/svg";
export const HTML_NS = "http://www.w3.org/1999/xhtml";

/** SVG elements that draw a shape: each must belong to a mark, an axis or decor. */
export const SHAPES: ReadonlySet<string> = new Set([
  "rect",
  "circle",
  "ellipse",
  "line",
  "path",
  "polygon",
  "polyline",
  "use",
]);

type ContentKind = Extract<Problem, { kind: "unsupported_content" }>["content"];

/** HTML elements whose text or picture the check cannot read. */
const UNSUPPORTED_HTML: Readonly<Record<string, ContentKind>> = {
  canvas: "canvas",
  img: "image",
  picture: "image",
  video: "media",
  audio: "media",
  iframe: "frame",
  frame: "frame",
  object: "object",
  embed: "object",
  input: "form_field",
  textarea: "form_field",
  select: "form_field",
};

/** SVG animation elements: they change the drawing after the check, like a CSS animation. */
const SMIL: ReadonlySet<string> = new Set(["animate", "animateMotion", "animateTransform", "set", "discard"]);

type AttributePlace = Exclude<(typeof LITERAL_PLACES)[number], "text" | "css-content" | "list-marker">;

/** Attributes a browser shows or reads aloud. */
export const TEXT_ATTRIBUTES: readonly AttributePlace[] = LITERAL_PLACES.filter(
  (place): place is AttributePlace => place !== "text" && place !== "css-content" && place !== "list-marker",
);

/**
 * List markers that are symbols, not numbers. Every other counter style counts as a number,
 * including the ones that write letters or roman numerals: a counter style falls back to
 * `decimal` outside its range (`lower-roman` above 3999, `lower-alpha` at 0 or below), the page
 * sets the `list-item` counter anywhere with `start`, `value`, `reversed` and
 * `counter-reset`/`counter-set`/`counter-increment`, and `@counter-style` can name any
 * symbols. No DOM API returns a marker's rendered text, so the comparison cannot tell which of
 * these show decimal digits.
 */
const SYMBOL_MARKERS: ReadonlySet<string> = new Set([
  "none",
  "disc",
  "circle",
  "square",
  "disclosure-open",
  "disclosure-closed",
]);

const PSEUDO_ELEMENTS = ["::before", "::after", "::marker"] as const;
const TAGS: ReadonlySet<string> = new Set(ELEMENT_TAGS);
const LITERAL = /\p{Nd}[\p{Nd}.,]*/u;
const LITERALS = /\p{Nd}[\p{Nd}.,]*/gu;

/** What the display-time comparison does about a finding. */
export type Fix =
  /** A text mark: its text becomes 「—」. */
  | { kind: "text"; element: Element }
  /** A graphic: it is shown as flagged. */
  | { kind: "graphic"; element: Element }
  /** A text node outside the marks: its numbers become 「—」. */
  | { kind: "data"; node: Node; element: Element | null }
  /** An attribute: its numbers become 「—」. */
  | { kind: "attribute"; element: Element; name: string }
  /** Generated content: the element's ::before, ::after and ::marker are turned off. */
  | { kind: "pseudo"; element: Element }
  /** Content the check cannot read: hidden. */
  | { kind: "hide"; element: Element };

export interface Finding {
  problem: Problem;
  fix: Fix | null;
}

export interface MarkReport {
  reference: string;
  value: ContractValue;
}

export interface Comparison {
  /** Every problem found, in document order, then the geometry and animation rules. */
  findings: Finding[];
  /** Every distinct marked reference with its value, in document order. */
  marks: MarkReport[];
}

/** The first number in `text` as a problem names it: digits and separators, at most 32 characters. */
export function literalOf(text: string): string | null {
  const match = LITERAL.exec(text);
  if (match === null) return null;
  const chars = Array.from(match[0]);
  return chars.length > MAX_LITERAL_CHARS ? chars.slice(0, MAX_LITERAL_CHARS).join("") : match[0];
}

/** `text` with every number replaced by 「—」, for display. */
export function withoutNumbers(text: string): string {
  return text.replace(LITERALS, "\u2014");
}

interface CssContent {
  strings: string[];
  /** open-quote or close-quote: the text comes from the `quotes` property. */
  quotes: boolean;
  /** Anything but quoted strings and keywords: counter(), attr(), url(), images. */
  other: boolean;
}

/** Reads a computed `content` or `quotes` value: its quoted strings, unescaped, and what else it has. */
export function parseCssContent(value: string): CssContent {
  const parsed: CssContent = { strings: [], quotes: false, other: false };
  let i = 0;
  while (i < value.length) {
    const c = value.charAt(i);
    if (/\s/.test(c) || c === "/") {
      i++;
    } else if (c === '"' || c === "'") {
      let text = "";
      i++;
      while (i < value.length && value.charAt(i) !== c) {
        if (value.charAt(i) === "\\") {
          const hex = /^[0-9a-fA-F]{1,6}/.exec(value.slice(i + 1, i + 7));
          if (hex !== null) {
            const code = Number.parseInt(hex[0], 16);
            text +=
              code === 0 || code > 0x10ffff || (code >= 0xd800 && code <= 0xdfff)
                ? "\uFFFD"
                : String.fromCodePoint(code);
            i += 1 + hex[0].length;
            if (/\s/.test(value.charAt(i))) i++;
          } else {
            text += value.charAt(i + 1);
            i += 2;
          }
        } else {
          text += value.charAt(i);
          i++;
        }
      }
      i++;
      parsed.strings.push(text);
    } else {
      const word = /^[A-Za-z-]+/.exec(value.slice(i))?.[0] ?? "";
      const keyword = word.toLowerCase();
      if (word !== "" && value.charAt(i + word.length) !== "(") {
        if (keyword === "open-quote" || keyword === "close-quote") parsed.quotes = true;
        else if (
          keyword !== "normal" &&
          keyword !== "none" &&
          keyword !== "no-open-quote" &&
          keyword !== "no-close-quote"
        )
          parsed.other = true;
        i += word.length;
      } else {
        parsed.other = true;
        return parsed;
      }
    }
  }
  return parsed;
}

/** `overflow` values that clip or scroll away the part of the content past the box. */
const CLIPPING: ReadonlySet<string> = new Set(["hidden", "clip", "scroll", "auto"]);
/** `overflow` values that clip without letting a reader scroll to the rest. */
const HIDING: ReadonlySet<string> = new Set(["hidden", "clip"]);

/** A computed value's rule: true when the value keeps the text plain. */
type Plain = (value: string) => boolean;

/**
 * One of these keywords. An empty value always passes: only a DOM without computed styles (a unit
 * test's) returns one, and a browser returns every property listed here.
 */
const oneOf =
  (...values: string[]): Plain =>
  (value) =>
    value === "" || values.includes(value);

/** A length of zero (`0px`). */
const zero: Plain = (value) => value === "" || pixels(value) === 0;

/**
 * The computed styles a text mark keeps plain (frontend/sandbox/README.md, 「檢查規則」): an
 * allowlist. Each property here can turn, reorder, reshape, copy, clip or hide the glyphs of a text
 * whose textContent is right, so it must keep its initial value or a value that does none of that;
 * anything else is a mismatch. Dashboards in zh-TW and en, and d3's output, need none of the refused
 * values. `PLAIN_PATH` applies to the mark and every ancestor (the property is inherited, or acts on
 * the descendants' drawing); `PLAIN_MARK` to the mark itself (inherited, so the mark's value is what
 * reaches its text, or it acts on the element's own text only).
 *
 * Surveyed in Chromium's computed style (every property `getComputedStyle` lists). Refused besides
 * the ones below, by other rules: `display: none`, `visibility`, faint paint (`color`,
 * `-webkit-text-fill-color`, `fill`, `fill-opacity`, `opacity`), `transform`, `rotate`, `scale`,
 * `zoom`, `perspective`, `transform-style` (the transform rule), `letter-spacing`, `font-size`,
 * `overflow` (the clipping rule), `position-anchor` (`unanchored`), `::first-letter` and `::first-line` (`PSEUDO_SAME`,
 * `PSEUDO_INITIAL`), `::selection` (`SELECTION_PLAIN`), `::details-content` (DETAILS_CONTENT, held
 * to PLAIN_PATH, the transform and opacity rules, and no clipping, as an ancestor), and text broken
 * over lines. Allowed, because
 * they keep each glyph, its order and its orientation: the font properties (`font-family`,
 * `font-weight`, `font-style`, `font-stretch`, `font-variant-*`, `font-feature-settings`,
 * `font-variation-settings`, `font-kerning`, `font-synthesis-*`, `font-optical-sizing`,
 * `font-palette`; the shapes a font gives its digits are a residual), `text-transform`,
 * `word-spacing`, `line-height`, `vertical-align`, `baseline-shift`, `dominant-baseline`,
 * `alignment-baseline`, `text-anchor`, `text-align`, `text-align-last`, `text-justify`,
 * `text-box-*`, `text-spacing-trim`, `text-autospace`, `white-space-collapse`, `text-wrap-*`,
 * `word-break`, `line-break`, `overflow-wrap`, `hyphens` and `hyphenate-*` (wrapping is judged from
 * the result; digits a `hyphenate-character` or `text-emphasis-style` draws anywhere are literal
 * numbers), `ruby-*`, `math-*`, `paint-order`, `text-rendering`, `-webkit-font-smoothing`,
 * `-webkit-locale`, backgrounds, borders, `box-shadow`, corner shapes, `isolation`,
 * `backface-visibility` (a turned-away mark is mirrored, which the transform rule refuses),
 * `backdrop-filter` (it filters what is behind the element), `will-change`, `z-index`, positioning
 * and `translate` (where a mark sits is a residual), flex and grid order, sizing, margins and
 * padding, `anchor-name`, `anchor-scope`, `position-try-*`, animations and transitions (a live one
 * is a render timeout), `view-transition-*` (the same), and the properties with no effect on
 * drawing (`cursor`, `pointer-events`, `user-select`, `touch-action`, `speak`, `reading-*`,
 * `interactivity`, `field-sizing`, `caret-*` on read-only text, scroll snapping and its margins).
 */
const PLAIN_PATH: readonly (readonly [string, Plain])[] = [
  // Orientation and order.
  ["writing-mode", oneOf("horizontal-tb")],
  ["direction", oneOf("ltr")],
  ["text-orientation", oneOf("mixed")],
  ["unicode-bidi", oneOf("normal", "embed", "isolate")],
  ["-webkit-rtl-ordering", oneOf("logical")],
  ["offset-path", oneOf("none")],
  // Clipped or not drawn while the box keeps its area.
  ["contain", (value) => !/\b(?:paint|content|strict)\b/.test(value)],
  ["content-visibility", oneOf("visible")],
  ["clip-path", oneOf("none")],
  ["mask-image", oneOf("none")],
  ["-webkit-mask-box-image-source", oneOf("none")],
  ["clip", oneOf("auto")],
  ["border-shape", oneOf("none")],
  ["-webkit-line-clamp", oneOf("none")],
  ["text-fit", oneOf("none")],
  ["position-visibility", oneOf("anchors-visible", "always")],
  ["position-area", oneOf("none")],
  // Replaced, copied, blurred or blended.
  ["content", oneOf("normal")],
  ["-webkit-box-reflect", oneOf("none")],
  ["filter", (value) => value === "" || value === "none" || /^(?:\s*opacity\([^()]*\))+\s*$/.test(value)],
  ["mix-blend-mode", oneOf("normal")],
  // Lines drawn through the text: an ancestor's decorations are drawn through its descendants' text
  // too (Chromium also reports them on the mark, as -webkit-text-decorations-in-effect).
  ["text-decoration-line", oneOf("none")],
  // Painted over: an outline drawn inside the box (a negative offset) covers the content.
  [
    "outline-offset",
    (value) => {
      const offset = pixels(value);
      return offset === null || offset >= 0;
    },
  ],
  // Editable text gets carets and spelling marks drawn over it.
  ["-webkit-user-modify", oneOf("read-only")],
];

/** The marks' rules, on the mark itself: its text's glyphs. */
const PLAIN_MARK: readonly (readonly [string, Plain])[] = [
  // The sandbox's styles isolate every mark from the bidi order of the text around it.
  ["unicode-bidi", oneOf("isolate")],
  ["-webkit-text-security", oneOf("none")],
  ["text-combine-upright", oneOf("none")],
  ["text-shadow", oneOf("none")],
  ["-webkit-text-stroke-width", zero],
  ["font-size-adjust", oneOf("none")],
  ["text-size-adjust", oneOf("auto", "none", "100%")],
  ["text-emphasis-style", oneOf("none")],
  ["-webkit-text-decorations-in-effect", oneOf("none")],
  ["initial-letter", oneOf("normal")],
  [
    "text-indent",
    (value) => {
      if (value === "") return true;
      const length = /^([+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?)(?:px|%)(?:\s|$)/.exec(value.trim());
      return length !== null && Number(length[1]) >= 0;
    },
  ],
];

/** Inherited properties a `::first-letter` or `::first-line` must take from its element unchanged. */
const PSEUDO_SAME = [
  "font-size",
  "font-size-adjust",
  "letter-spacing",
  "visibility",
  "color",
  "-webkit-text-fill-color",
  "-webkit-text-stroke-width",
  "-webkit-text-security",
  "text-shadow",
  "text-emphasis-style",
] as const;

/** Properties that do not inherit, with the value a `::first-letter` or `::first-line` keeps. */
const PSEUDO_INITIAL: readonly (readonly [string, Plain])[] = [
  ["opacity", oneOf("1")],
  ["filter", oneOf("none")],
  ["transform", oneOf("none")],
  ["rotate", oneOf("none")],
  ["scale", oneOf("none")],
  ["translate", oneOf("none")],
  ["float", oneOf("none")],
  ["vertical-align", oneOf("baseline")],
  ["text-decoration-line", oneOf("none")],
  ["initial-letter", oneOf("normal")],
  ["margin-top", zero],
  ["margin-right", zero],
  ["margin-bottom", zero],
  ["margin-left", zero],
  ["padding-top", zero],
  ["padding-right", zero],
  ["padding-bottom", zero],
  ["padding-left", zero],
  ["border-top-width", zero],
  ["border-right-width", zero],
  ["border-bottom-width", zero],
  ["border-left-width", zero],
];

/** `overflow` that clips nothing, for a box the comparison cannot read. */
const UNCLIPPED: readonly (readonly [string, Plain])[] = [
  ["overflow-x", oneOf("visible")],
  ["overflow-y", oneOf("visible")],
];

/** What a `::selection` may change of a mark's text: its colours, as long as they are not faint. */
const SELECTION_PLAIN: readonly (readonly [string, Plain])[] = [
  ["text-shadow", oneOf("none")],
  ["-webkit-text-stroke-width", zero],
  ["text-decoration-line", oneOf("none")],
];

/** Whether `element`'s computed style keeps every rule of `rules`. */
function keeps(dom: DomAccess, element: Element, pseudo: string | null, rules: readonly (readonly [string, Plain])[]) {
  for (const [property, plain] of rules) if (!plain(dom.style(element, pseudo, property).trim())) return false;
  return true;
}

/** `position` values that make a box absolutely positioned: the boxes anchor positioning applies to. */
const ABSOLUTE: ReadonlySet<string> = new Set(["absolute", "fixed"]);

/**
 * Whether `position-anchor` leaves the box unanchored. It places an absolutely positioned box
 * against another box and, through `position-visibility`, hides it with that box, so such a box
 * keeps `normal` (Chromium's initial value) or `none`. `auto` (the implicit anchor, and the initial
 * value in browsers that follow an earlier draft) also passes on any other box, which anchor
 * positioning neither moves nor hides.
 */
function unanchored(dom: DomAccess, element: Element, pseudo: string | null): boolean {
  const anchor = dom.style(element, pseudo, "position-anchor").trim();
  if (anchor === "" || anchor === "normal" || anchor === "none") return true;
  return anchor === "auto" && !ABSOLUTE.has(dom.style(element, pseudo, "position").trim());
}

/** Whether the element, or its pseudo-element, keeps PLAIN_PATH and is unanchored. */
function pathPlain(dom: DomAccess, element: Element, pseudo: string | null): boolean {
  return keeps(dom, element, pseudo, PLAIN_PATH) && unanchored(dom, element, pseudo);
}

/**
 * Whether the element's own `::first-letter` and `::first-line` are left as they are: what they
 * draw is the first letter and line of the element's text, and of every descendant's on that line,
 * marks included.
 */
function pseudoPlain(dom: DomAccess, element: Element): boolean {
  const own = PSEUDO_SAME.map((property) => dom.style(element, null, property));
  for (const pseudo of ["::first-letter", "::first-line"] as const) {
    for (let i = 0; i < PSEUDO_SAME.length; i++) {
      if (dom.style(element, pseudo, PSEUDO_SAME[i] ?? "") !== own[i]) return false;
    }
    if (!keeps(dom, element, pseudo, PSEUDO_INITIAL)) return false;
  }
  return true;
}

/** Characters that reorder the text after them: bidi embeddings, overrides, isolates and marks. */
const BIDI_CONTROLS = /[\u061C\u200E\u200F\u202A-\u202E\u2066-\u2069]/u;

/**
 * Text painted fainter than this is hidden: the alpha of its paint (the faintest of `color`,
 * `-webkit-text-fill-color` and, in SVG, `fill` with `fill-opacity`) times the `opacity` of the
 * mark and of every ancestor. At a tenth of full strength no digit reads on any background.
 */
export const MIN_TEXT_ALPHA = 0.1;

/** Functions a browser serializes a computed colour with; their alpha is read, not guessed. */
const COLOR_FUNCTIONS: ReadonlySet<string> = new Set([
  "rgb",
  "rgba",
  "hsl",
  "hsla",
  "hwb",
  "lab",
  "lch",
  "oklab",
  "oklch",
  "color",
]);
/** The legacy syntax `rgba(r, g, b, a)`: the alpha is the fourth comma-separated value. */
const COMMA_COLOR_FUNCTIONS: ReadonlySet<string> = new Set(["rgb", "rgba", "hsl", "hsla"]);
const CSS_NUMBER = /^[+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?%?$/;

/** A CSS number or percentage as a fraction clamped to 0 to 1, or `null` when it is not one. */
function fraction(token: string): number | null {
  if (!CSS_NUMBER.test(token)) return null;
  const value = Number.parseFloat(token) / (token.endsWith("%") ? 100 : 1);
  return Number.isFinite(value) ? Math.min(1, Math.max(0, value)) : null;
}

/**
 * The alpha (0 to 1) of a computed colour, in every syntax a browser serializes one with:
 * `rgb()`/`rgba()` with commas, and `color()`, `lab()`, `lch()`, `oklab()`, `oklch()` (and the
 * others) with `/ alpha`; `transparent`, `none`, `context-fill` and `context-stroke` are 0, and so
 * is an alpha written `none`. Any other function (a gradient's `url()`, too), or an alpha that is
 * not a plain number (fail closed), is 0. `null` when the value names no colour the comparison can
 * read: empty, or another keyword left as written (`red`, `currentColor`), which only a DOM without
 * computed colours returns (a browser resolves them).
 */
export function colorAlpha(value: string): number | null {
  const alpha = readAlpha(value);
  return alpha === undefined ? 0 : alpha;
}

/**
 * colorAlpha, except that a value it reads as 0 only to fail closed (another function, an alpha
 * that is not a plain number) is `undefined`: a paint whose absence would let text through (a
 * stroke over the glyphs) must not count that as transparent.
 */
function readAlpha(value: string): number | null | undefined {
  const text = value.trim().toLowerCase();
  if (text === "") return null;
  // `context-fill` and `context-stroke` paint nothing outside a marker or a `use`.
  if (text === "transparent" || text === "none" || text === "context-fill" || text === "context-stroke") return 0;
  const hex = /^#([0-9a-f]{3,4}|[0-9a-f]{6}|[0-9a-f]{8})$/.exec(text)?.[1];
  if (hex !== undefined) {
    if (hex.length === 3 || hex.length === 6) return 1;
    const digits = hex.length === 4 ? `${hex.slice(3)}${hex.slice(3)}` : hex.slice(6);
    return Number.parseInt(digits, 16) / 255;
  }
  const call = /^([a-z-]+)\(([^()]*)\)$/.exec(text);
  if (call === null) return /^[a-z-]+$/.test(text) ? null : undefined;
  const name = call[1] ?? "";
  const args = call[2] ?? "";
  if (!COLOR_FUNCTIONS.has(name)) return undefined;
  let token: string;
  const slash = args.lastIndexOf("/");
  if (slash >= 0) {
    token = args.slice(slash + 1).trim();
  } else if (COMMA_COLOR_FUNCTIONS.has(name) && args.includes(",")) {
    const parts = args.split(",");
    if (parts.length === 3) return 1;
    if (parts.length !== 4) return undefined;
    token = (parts[3] ?? "").trim();
  } else {
    return 1;
  }
  if (token === "none") return 0;
  return fraction(token) ?? undefined;
}

/** A computed `opacity` or `fill-opacity` as 0 to 1: 1 when empty (no computed styles), 0 when unreadable. */
function opacityOf(value: string): number {
  const text = value.trim();
  return text === "" ? 1 : (fraction(text) ?? 0);
}

/** The alpha a mark's text, or its pseudo-element's, is painted with: the faintest of its paints. */
function paintAlpha(dom: DomAccess, element: Element, pseudo: string | null, svg: boolean): number {
  const paints = [colorAlpha(dom.style(element, pseudo, "color"))];
  // Chromium paints HTML text with -webkit-text-fill-color, which defaults to the colour; SVG text
  // with its fill.
  paints.push(colorAlpha(dom.style(element, pseudo, "-webkit-text-fill-color")));
  if (svg && pseudo === null) {
    const fill = colorAlpha(dom.style(element, null, "fill"));
    paints.push(fill === null ? null : fill * opacityOf(dom.style(element, null, "fill-opacity")));
  }
  let alpha = 1;
  for (const paint of paints) if (paint !== null) alpha = Math.min(alpha, paint);
  return alpha;
}

/** The product of the `opacity()` functions in a computed `filter`; 1 for `none`. */
function filterOpacity(value: string): number {
  let product = 1;
  const pattern = /(?:^|[\s)])opacity\(([^()]*)\)/g;
  const text = value.toLowerCase();
  for (let match = pattern.exec(text); match !== null; match = pattern.exec(text)) {
    const argument = (match[1] ?? "").trim();
    product *= argument === "" ? 1 : (fraction(argument) ?? 0);
  }
  return product;
}

/**
 * Text turned further than this from upright, either way, can read as other digits (a 6 for a 9
 * once a reader takes it for sideways text); slanted tick labels (d3's usual −45°) stay within it.
 */
export const MAX_TEXT_TURN_DEG = 45;
/** Text stretched more than this many times in one direction than in another is squeezed past reading. */
export const MAX_TEXT_STRETCH = 4;
/**
 * Text drawn smaller than this many CSS pixels (its font size times its scale on screen, zoom
 * included) shows nothing readable. Small enough that a chart scaled down to fit a phone still
 * passes.
 */
export const MIN_TEXT_SIZE_PX = 2;
/** Letter spacing tighter than this many em runs the digits into each other. */
export const MIN_LETTER_SPACING_EM = -0.2;
/**
 * A pie slice's angle reads true only when its chart is scaled alike in every direction: up to the
 * geometry's own tolerance.
 */
export const MAX_SLICE_STRETCH = 1 + RELATIVE_TOLERANCE;
/** Rounding in a browser's matrices (single precision in SVG), so a −45° label is not judged −45.00001°. */
const TURN_SLACK_DEG = 0.01;

/**
 * The linear part `[a, b, c, d]` of a CSS `matrix(a, b, c, d, e, f)`: how an element is turned,
 * mirrored, stretched and scaled on screen (a translation does not change how text reads).
 */
export type Linear = readonly [number, number, number, number];

const IDENTITY: Linear = [1, 0, 0, 1];

/** `m · n`: `n` applies first, as an element's own transform applies before its parent's. */
function multiply(m: Linear, n: Linear): Linear {
  return [m[0] * n[0] + m[2] * n[1], m[1] * n[0] + m[3] * n[1], m[0] * n[2] + m[2] * n[3], m[1] * n[2] + m[3] * n[3]];
}

const CSS_PLAIN_NUMBER = /^[+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?$/;

/** Numbers separated by commas or spaces; `null` when any is not one. */
function numberList(text: string): number[] | null {
  const numbers: number[] = [];
  for (const token of text.trim().split(/\s*,\s*|\s+/)) {
    if (!CSS_PLAIN_NUMBER.test(token)) return null;
    numbers.push(Number(token));
  }
  return numbers;
}

/** A 3 × 3 linear map, row-major: what a transform does to x, y and z before it is flattened. */
type Linear3 = readonly [number, number, number, number, number, number, number, number, number];

const IDENTITY3: Linear3 = [1, 0, 0, 0, 1, 0, 0, 0, 1];

function multiply3(m: Linear3, n: Linear3): Linear3 {
  const out: number[] = [];
  for (let row = 0; row < 3; row++) {
    for (let column = 0; column < 3; column++) {
      let sum = 0;
      for (let k = 0; k < 3; k++) sum += (m[row * 3 + k] ?? 0) * (n[k * 3 + column] ?? 0);
      out.push(sum);
    }
  }
  return out as unknown as Linear3;
}

/** What the screen shows of a 3 × 3 map: its x and y rows and columns (an orthographic flattening). */
function flatten(m: Linear3): Linear {
  return [m[0], m[3], m[1], m[4]];
}

/**
 * A computed `transform`: `none`, `matrix(...)` or `matrix3d(...)` (its linear part over `m44`);
 * `null` for anything else, and for a `matrix3d` with perspective or a `m44` that is not
 * positive, whose picture no linear map describes (it can be turned over).
 */
function transformLinear(value: string): Linear3 | null {
  const text = value.trim().toLowerCase();
  if (text === "" || text === "none") return IDENTITY3;
  const call = /^(matrix|matrix3d)\(([^()]*)\)$/.exec(text);
  const v = call === null ? null : numberList(call[2] ?? "");
  if (call === null || v === null) return null;
  if (call[1] === "matrix") {
    const [a, b, c, d] = v;
    if (v.length !== 6 || a === undefined || b === undefined || c === undefined || d === undefined) return null;
    return [a, c, 0, b, d, 0, 0, 0, 1];
  }
  // matrix3d is column-major: m11 m12 m13 m14 m21 ... m44; m14, m24, m34 are perspective.
  if (v.length !== 16 || v[3] !== 0 || v[7] !== 0 || v[11] !== 0) return null;
  const w = v[15] ?? Number.NaN;
  if (!(w > 0)) return null;
  const at = (i: number) => (v[i] ?? Number.NaN) / w;
  return [at(0), at(4), at(8), at(1), at(5), at(9), at(2), at(6), at(10)];
}

/** A CSS angle in degrees; `null` when it is not one. */
function degreesOf(token: string): number | null {
  const match = /^([+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?)(deg|grad|rad|turn)?$/.exec(token);
  if (match === null) return null;
  const value = Number(match[1]);
  const unit = match[2];
  if (unit === undefined) return value === 0 ? 0 : null;
  return value * (unit === "deg" ? 1 : unit === "grad" ? 0.9 : unit === "rad" ? 180 / Math.PI : 360);
}

/** A computed `rotate`: `none`, an angle, or an axis (`x`, `y`, `z` or three numbers) and an angle. */
function rotateLinear(value: string): Linear3 | null {
  const text = value.trim().toLowerCase();
  if (text === "" || text === "none") return IDENTITY3;
  const tokens = text.split(/\s+/);
  const angle = degreesOf(tokens[tokens.length - 1] ?? "");
  if (angle === null) return null;
  const named = (name: string | undefined) =>
    name === "x" ? [1, 0, 0] : name === "y" ? [0, 1, 0] : name === "z" ? [0, 0, 1] : null;
  const axis =
    tokens.length === 1
      ? [0, 0, 1]
      : tokens.length === 2
        ? named(tokens[0])
        : tokens.length === 4
          ? numberList(tokens.slice(0, 3).join(" "))
          : null;
  if (axis === null) return null;
  const [x = 0, y = 0, z = 0] = axis;
  const length = Math.hypot(x, y, z);
  // A zero axis turns nothing (CSS Transforms 2).
  if (!(length > 0)) return IDENTITY3;
  const [ux, uy, uz] = [x / length, y / length, z / length];
  const radians = (angle * Math.PI) / 180;
  const cos = Math.cos(radians);
  const sin = Math.sin(radians);
  const t = 1 - cos;
  // The rotation about the axis (Rodrigues), clockwise on screen for a positive angle about z.
  return [
    cos + ux * ux * t,
    ux * uy * t - uz * sin,
    ux * uz * t + uy * sin,
    uy * ux * t + uz * sin,
    cos + uy * uy * t,
    uy * uz * t - ux * sin,
    uz * ux * t - uy * sin,
    uz * uy * t + ux * sin,
    cos + uz * uz * t,
  ];
}

/** A computed `scale`: `none`, or one to three factors (x, y, z; y defaults to x, z to 1). */
function scaleLinear(value: string): Linear3 | null {
  const text = value.trim().toLowerCase();
  if (text === "" || text === "none") return IDENTITY3;
  const factors: number[] = [];
  for (const token of text.split(/\s+/)) {
    if (!CSS_NUMBER.test(token)) return null;
    factors.push(Number.parseFloat(token) / (token.endsWith("%") ? 100 : 1));
  }
  const [x, y = x, z = 1] = factors;
  return factors.length > 3 || x === undefined || y === undefined ? null : [x, 0, 0, 0, y, 0, 0, 0, z];
}

/** A computed `zoom` as a factor: 1 when empty or `normal`, `null` when unreadable. */
function zoomOf(value: string): number | null {
  const text = value.trim().toLowerCase();
  if (text === "" || text === "normal") return 1;
  if (!CSS_NUMBER.test(text)) return null;
  const zoom = Number.parseFloat(text) / (text.endsWith("%") ? 100 : 1);
  return zoom > 0 ? zoom : null;
}

/**
 * The pseudo-element a `<details>` element draws its content in (every child but its summary): it
 * is styled like an element of its own, and its styles are no ancestor's in the DOM.
 */
const DETAILS_CONTENT = "::details-content";

/**
 * An element's own transform on screen, or its pseudo-element's: `rotate`, `scale` and `transform`
 * composed in 3D (CSS Transforms 2 applies `translate`, `rotate`, `scale`, `transform` in that
 * order, outermost first) and flattened, and the element's `zoom`, which scales alike in every
 * direction. `null` when one of them could not be read, or when the element sets a `perspective`
 * or keeps its children in 3D (`transform-style: preserve-3d`): flattening each element into its
 * parent's plane is then no longer what the screen shows. DETAILS_CONTENT, which holds elements,
 * is read like an element; a text pseudo-element (`::first-letter`) has no zoom or children.
 */
function ownLinear(dom: DomAccess, element: Element, pseudo: string | null): Linear | null {
  const holder = pseudo === null || pseudo === DETAILS_CONTENT;
  const rotate = rotateLinear(dom.style(element, pseudo, "rotate"));
  const scale = scaleLinear(dom.style(element, pseudo, "scale"));
  const transform = transformLinear(dom.style(element, pseudo, "transform"));
  const zoom = holder ? zoomOf(dom.style(element, pseudo, "zoom")) : 1;
  if (rotate === null || scale === null || transform === null || zoom === null) return null;
  if (holder) {
    const perspective = dom.style(element, pseudo, "perspective").trim();
    if (perspective !== "" && perspective !== "none") return null;
    if (dom.style(element, pseudo, "transform-style").trim() === "preserve-3d") return null;
  }
  const [a, b, c, d] = flatten(multiply3(multiply3(rotate, scale), transform));
  return [zoom * a, zoom * b, zoom * c, zoom * d];
}

/** How much `m` scales areas: its determinant (negative when it mirrors). */
function determinantOf(m: Linear): number {
  return m[0] * m[3] - m[1] * m[2];
}

/** How much `m` stretches one direction more than another (the ratio of its singular values). */
export function stretchOf(m: Linear): number {
  const [a, b, c, d] = m;
  const sum = a * a + b * b + c * c + d * d;
  const determinant = a * d - b * c;
  const root = Math.sqrt(Math.max(0, sum * sum - 4 * determinant * determinant));
  const small = (sum - root) / 2;
  return small > 0 ? Math.sqrt((sum + root) / 2 / small) : Number.POSITIVE_INFINITY;
}

/**
 * Whether text drawn through `m` can read as other text: mirrored (a negative determinant; none
 * collapses it), turned past MAX_TEXT_TURN_DEG, or stretched past MAX_TEXT_STRETCH. Translation and
 * uniform scaling read the same. `null` (a transform that could not be read) is judged misread.
 */
export function misreads(m: Linear | null): boolean {
  if (m === null) return true;
  const [a, b, c, d] = m;
  if (!(a * d - b * c > 0)) return true;
  // The turn of m's rotation part (its polar decomposition); for a rotation by θ it is θ.
  const turn = (Math.atan2(b - c, a + d) * 180) / Math.PI;
  return !(Math.abs(turn) <= MAX_TEXT_TURN_DEG + TURN_SLACK_DEG) || !(stretchOf(m) <= MAX_TEXT_STRETCH);
}

/** How an element is presented through its ancestors. */
export interface Presentation {
  /** The product of its own and its ancestors' opacity (`opacity`, and `opacity()` in `filter`). */
  opacity: number;
  /** How it is turned, mirrored, stretched and scaled on screen; `null` when a transform could not be read. */
  linear: Linear | null;
  /**
   * Whether it and every ancestor keep PLAIN_PATH and leave their `::first-letter` and
   * `::first-line` as they are.
   */
  plain: boolean;
  /**
   * With layout, the area its content stays in: the padding boxes of it and its ancestors whose
   * `overflow` cuts off what lies past them (`hidden`, `clip`; one axis or both), intersected
   * (NOWHERE under one whose clip no rectangle reads, `clipArea`). `null` when none does. The root
   * and `<body>` are left out: their `overflow` applies to the viewport.
   */
  inside: Area | null;
}

/** A rectangle in the viewport, by its edges. */
export interface Area {
  left: number;
  top: number;
  right: number;
  bottom: number;
}

const UNBOUNDED: Area = {
  left: Number.NEGATIVE_INFINITY,
  top: Number.NEGATIVE_INFINITY,
  right: Number.POSITIVE_INFINITY,
  bottom: Number.POSITIVE_INFINITY,
};

/** An area no box lies inside: where the content of a box whose clip cannot be read stays. */
const NOWHERE: Area = {
  left: Number.POSITIVE_INFINITY,
  top: Number.POSITIVE_INFINITY,
  right: Number.NEGATIVE_INFINITY,
  bottom: Number.NEGATIVE_INFINITY,
};

/** Rounding in a browser's layout: a box this close to its clipping edge is inside it. */
const EDGE_SLACK_PX = 1;

/**
 * The area an element's `overflow` keeps its content in, in the viewport: its padding box, which is
 * its border box `box` less its borders as `linear` draws them. `null` when no rectangle reads it:
 * the element is turned, skewed or mirrored on screen (its bounding box is then larger than what it
 * clips to), its transform is unknown, or a border width cannot be read.
 */
function clipArea(dom: DomAccess, element: Element, box: Box, linear: Linear | null): Area | null {
  if (linear === null) return null;
  const [a, b, c, d] = linear;
  const tilt = 1e-9 * Math.max(Math.abs(a), Math.abs(d));
  if (!(a > 0 && d > 0 && Math.abs(b) <= tilt && Math.abs(c) <= tilt)) return null;
  const border = (side: string) => {
    const width = pixels(dom.style(element, null, `border-${side}-width`));
    // A DOM without computed styles has no borders to read.
    return width === null ? 0 : width;
  };
  const left = border("left") * a;
  const right = border("right") * a;
  const top = border("top") * d;
  const bottom = border("bottom") * d;
  if (!(Number.isFinite(left) && Number.isFinite(right) && Number.isFinite(top) && Number.isFinite(bottom))) {
    return null;
  }
  return {
    left: box.left + left,
    top: box.top + top,
    right: box.left + box.width - right,
    bottom: box.top + box.height - bottom,
  };
}

/**
 * Reads the presentation of elements, each ancestor once: a document has up to MAX_ELEMENTS
 * elements and MAX_MARKS marks, so the walk up from every mark shares what it read. One per
 * comparison (the document does not change while it runs). With layout, an SVG element's
 * transform is its screen matrix, which holds every ancestor's transform (HTML ones included) and
 * its `<svg>`'s viewBox; any other element adds its own transform to its parent's.
 */
export function presenter(dom: DomAccess, layout: boolean): (element: Element) => Presentation {
  const known = new Map<Element, Presentation>();
  const root = dom.documentElement();
  /** The root and `<body>`: their `overflow` is the viewport's, which a frame grows to fit. */
  const viewport = (element: Element) => {
    if (element === root) return true;
    const parent = dom.parentNode(element);
    return parent === root && dom.localName(element) === "body" && dom.namespaceURI(element) === HTML_NS;
  };
  /**
   * `base` passed through the DETAILS_CONTENT of `element`'s parent, when the parent is a
   * `<details>` that draws `element` in it: its opacity, transform and PLAIN_PATH apply as an
   * ancestor's would, and it must not clip (its box cannot be read).
   */
  const throughDetails = (element: Element, base: Presentation): Presentation => {
    const parent = dom.parentNode(element);
    if (parent === null || dom.nodeType(parent) !== 1) return base;
    const details = parent as Element;
    if (dom.localName(details) !== "details" || dom.namespaceURI(details) !== HTML_NS) return base;
    for (let child = dom.firstChild(details); child !== null; child = dom.nextSibling(child)) {
      if (dom.nodeType(child) !== 1) continue;
      const summary = dom.localName(child as Element) === "summary" && dom.namespaceURI(child as Element) === HTML_NS;
      if (summary) {
        if (child === element) return base;
        break;
      }
    }
    const own = base.linear === null ? null : ownLinear(dom, details, DETAILS_CONTENT);
    return {
      opacity:
        base.opacity *
        opacityOf(dom.style(details, DETAILS_CONTENT, "opacity")) *
        filterOpacity(dom.style(details, DETAILS_CONTENT, "filter")),
      linear: base.linear === null || own === null ? null : multiply(base.linear, own),
      plain: base.plain && pathPlain(dom, details, DETAILS_CONTENT) && keeps(dom, details, DETAILS_CONTENT, UNCLIPPED),
      inside: base.inside,
    };
  };
  return (element) => {
    // Iterative, from the nearest known ancestor down: a deep document must not exhaust the stack.
    const chain: Element[] = [];
    let base: Presentation = { opacity: 1, linear: IDENTITY, plain: true, inside: null };
    for (let node: Node | null = element; node !== null && dom.nodeType(node) === 1; node = dom.parentNode(node)) {
      const found = known.get(node as Element);
      if (found !== undefined) {
        base = found;
        break;
      }
      chain.push(node as Element);
    }
    for (let i = chain.length - 1; i >= 0; i--) {
      const current = chain[i] as Element;
      base = throughDetails(current, base);
      const opacity =
        base.opacity *
        opacityOf(dom.style(current, null, "opacity")) *
        filterOpacity(dom.style(current, null, "filter"));
      const screen = layout && dom.namespaceURI(current) === SVG_NS ? dom.screenMatrix(current) : null;
      let linear: Linear | null = null;
      if (base.linear !== null) {
        if (screen !== null) {
          linear = [screen.a, screen.b, screen.c, screen.d];
        } else {
          const own = ownLinear(dom, current, null);
          linear = own === null ? null : multiply(base.linear, own);
        }
      }
      const plain = base.plain && pathPlain(dom, current, null);
      let inside = base.inside;
      if (layout && !viewport(current)) {
        const clipsX = HIDING.has(dom.style(current, null, "overflow-x"));
        const clipsY = HIDING.has(dom.style(current, null, "overflow-y"));
        if (clipsX || clipsY) {
          const clip = clipArea(dom, current, dom.box(current), linear);
          const area = inside ?? UNBOUNDED;
          inside =
            clip === null
              ? NOWHERE
              : {
                  left: clipsX ? Math.max(area.left, clip.left) : area.left,
                  right: clipsX ? Math.min(area.right, clip.right) : area.right,
                  top: clipsY ? Math.max(area.top, clip.top) : area.top,
                  bottom: clipsY ? Math.min(area.bottom, clip.bottom) : area.bottom,
                };
        }
      }
      base = { opacity, linear, plain: plain && pseudoPlain(dom, current), inside };
      known.set(current, base);
    }
    return base;
  };
}

const SVG_TEXT: ReadonlySet<string> = new Set(["text", "tspan", "textPath"]);

/**
 * Whether SVG text places its glyphs one by one: a list of `x`, `y`, `dx` or `dy` positions (which
 * can reorder or stack the digits), `rotate` (turns each glyph), `textLength` (squeezes them) or a
 * `textPath` (bends them along a path), on the mark or the text it is part of.
 */
function placesGlyphs(dom: DomAccess, element: Element): boolean {
  for (let node: Node | null = element; node !== null && dom.nodeType(node) === 1; node = dom.parentNode(node)) {
    const current = node as Element;
    const name = dom.localName(current);
    if (dom.namespaceURI(current) !== SVG_NS || !SVG_TEXT.has(name)) return false;
    if (name === "textPath") return true;
    if (dom.getAttribute(current, "rotate") !== null || dom.getAttribute(current, "textLength") !== null) return true;
    for (const place of ["x", "y", "dx", "dy"]) {
      const value = dom.getAttribute(current, place);
      if (value !== null && value.trim().split(/[\s,]+/).length > 1) return true;
    }
  }
  return false;
}

/** A computed length in pixels (`normal` letter spacing is 0); `null` when empty, `NaN` when unreadable. */
function pixels(value: string): number | null {
  const text = value.trim().toLowerCase();
  if (text === "") return null;
  if (text === "normal") return 0;
  return text.endsWith("px") && CSS_PLAIN_NUMBER.test(text.slice(0, -2)) ? Number(text.slice(0, -2)) : Number.NaN;
}

/** Whether letter spacing runs the digits into each other (tighter than MIN_LETTER_SPACING_EM). */
function cramped(dom: DomAccess, element: Element, pseudo: string | null, fontSize: string): boolean {
  const spacing = pixels(dom.style(element, pseudo, "letter-spacing"));
  const size = pixels(fontSize);
  if (spacing === null || size === null) return false;
  return !(spacing >= MIN_LETTER_SPACING_EM * size);
}

/**
 * Whether SVG text is stroked: a stroke of some width draws over the glyphs' fill. The stroke is
 * absent only when its width is 0, or its paint (`none` included) or `stroke-opacity` is read as
 * fully transparent; a gradient or pattern (`url()`), or a value that cannot be read, is drawn (fail
 * closed). An empty `stroke` (a DOM without computed styles) is absent.
 */
function stroked(dom: DomAccess, element: Element): boolean {
  const stroke = dom.style(element, null, "stroke").trim().toLowerCase();
  // The keywords that paint nothing: SVG text's initial `none`, and `transparent`. readAlpha reads
  // both as 0 too; they are named here so the common case does not depend on it.
  if (stroke === "" || stroke === "none" || stroke === "transparent") return false;
  if (pixels(dom.style(element, null, "stroke-width")) === 0) return false;
  const opacity = dom.style(element, null, "stroke-opacity").trim();
  return !(readAlpha(stroke) === 0 || (opacity !== "" && fraction(opacity) === 0));
}

/**
 * Whether the fragments a text is drawn in leave its line: one starts above or below the first, or
 * left of where the previous one ends (a new line, or glyphs reordered or run into each other).
 */
function broken(boxes: readonly Box[]): boolean {
  const drawn = boxes.filter((box) => box.width > 0 || box.height > 0);
  const first = drawn[0];
  if (first === undefined) return false;
  for (let i = 1; i < drawn.length; i++) {
    const previous = drawn[i - 1] as Box;
    const current = drawn[i] as Box;
    if (Math.abs(current.top - first.top) > EDGE_SLACK_PX) return true;
    if (current.left < previous.left + previous.width - EDGE_SLACK_PX) return true;
  }
  return false;
}

/** Whether `box` lies inside `area`, up to EDGE_SLACK_PX. */
function inArea(box: Box, area: Area): boolean {
  return (
    box.left >= area.left - EDGE_SLACK_PX &&
    box.top >= area.top - EDGE_SLACK_PX &&
    box.left + box.width <= area.right + EDGE_SLACK_PX &&
    box.top + box.height <= area.bottom + EDGE_SLACK_PX
  );
}

/**
 * Whether a text mark's rendered text may differ from its `textContent`, so comparing the two is
 * not enough (frontend/sandbox/README.md, 「檢查規則」). A text mark is one text node, no element
 * children, shown whole, upright, in order and on one line. Flags: an element child (its own text
 * or styling changes the number); a mark that is not drawn (`display: none` or `contents`, which
 * has no box to isolate its text, `visibility: hidden`, content the browser skips, a paint fainter
 * than MIN_TEXT_ALPHA through its colours and the opacity of the mark and its ancestors); a
 * transform of the mark or an ancestor that mirrors it, turns it past MAX_TEXT_TURN_DEG or
 * stretches it past MAX_TEXT_STRETCH; SVG text that places its glyphs one by one or is stroked;
 * letter spacing that runs the digits together; glyphs drawn smaller than MIN_TEXT_SIZE_PX; a
 * computed style outside the allowlist (PLAIN_PATH on the mark and its ancestors, PLAIN_MARK on the
 * mark), a restyled `::first-letter`/`::first-line` of the mark or an ancestor, or a `::selection`
 * that hides or copies the text; a `<details>` mark (its text is drawn in DETAILS_CONTENT); bidi
 * control characters in its text; and, with layout, a box with
 * no area (`font-size: 0`), a box its own `overflow` clips (an ellipsis is this with
 * `text-overflow` set) or that lies outside an ancestor's clipping box, and a number broken over
 * lines. The layout checks, and an SVG element's screen matrix, are used only when the caller says
 * it has layout (`layout`: the checker's browser and the runtime's, never a unit test's DOM).
 * `present` reads the presentation through the ancestors; a comparison passes one it shares between
 * marks.
 */
export function concealsText(
  dom: DomAccess,
  element: Element,
  layout: boolean,
  present: (element: Element) => Presentation = presenter(dom, layout),
): boolean {
  const texts: Node[] = [];
  for (let child = dom.firstChild(element); child !== null; child = dom.nextSibling(child)) {
    const type = dom.nodeType(child);
    if (type === 1) return true;
    if (type === 3 || type === 4) texts.push(child);
  }
  const display = dom.style(element, null, "display");
  if (display === "none" || display === "contents") return true;
  // A <details> draws its text in DETAILS_CONTENT, styled apart from the element.
  if (dom.localName(element) === "details" && dom.namespaceURI(element) === HTML_NS) return true;
  const visibility = dom.style(element, null, "visibility");
  if (visibility === "hidden" || visibility === "collapse") return true;
  const svg = dom.namespaceURI(element) === SVG_NS;
  const { opacity, linear, plain, inside } = present(element);
  if (!(paintAlpha(dom, element, null, svg) * opacity >= MIN_TEXT_ALPHA)) return true;
  if (linear === null || misreads(linear)) return true;
  if (svg && (placesGlyphs(dom, element) || stroked(dom, element))) return true;
  const own = dom.style(element, null, "font-size");
  // The size the glyphs are drawn at: a uniform scale is allowed, down to a readable size.
  const fontSize = pixels(own);
  if (fontSize !== null && !(fontSize * Math.sqrt(Math.abs(determinantOf(linear))) >= MIN_TEXT_SIZE_PX)) {
    return true;
  }
  if (cramped(dom, element, null, own)) return true;
  if (!plain || !keeps(dom, element, null, PLAIN_MARK)) return true;
  // `text-indent` (inherited) moves the first line of a block container's own text, as far as it
  // says: an HTML mark that is not inline is one, and keeps it at 0. SVG text has no indent; a DOM
  // without computed styles has no display to read.
  const shown = dom.style(element, null, "display").trim();
  if (!svg && shown !== "" && shown !== "inline" && !zero(dom.style(element, null, "text-indent").trim())) {
    return true;
  }
  const text = dom.textContent(element);
  if (BIDI_CONTROLS.test(text)) return true;
  // A selection, the page's own or a reader's, is drawn with ::selection: it may change the colour.
  if (!(paintAlpha(dom, element, "::selection", false) >= MIN_TEXT_ALPHA)) return true;
  if (!keeps(dom, element, "::selection", SELECTION_PLAIN)) return true;
  if (layout) {
    if (!dom.visible(element)) return true;
    // A box with no area shows nothing (font-size: 0).
    const box = dom.box(element);
    if (!(box.width > 0 && box.height > 0)) return true;
    const scroll = dom.scrollSize(element);
    if (CLIPPING.has(dom.style(element, null, "overflow-x")) && scroll.scrollWidth - scroll.clientWidth > 1) {
      return true;
    }
    if (CLIPPING.has(dom.style(element, null, "overflow-y")) && scroll.scrollHeight - scroll.clientHeight > 1) {
      return true;
    }
    if (inside !== null && !inArea(box, inside)) return true;
    // A number broken over lines reads as two; text without digits may wrap.
    if (LITERAL.test(text)) {
      const boxes: Box[] = [];
      for (const node of texts) {
        // More fragments than the DOM reads: a number in that many pieces is broken.
        const read = dom.textBoxes(node);
        if (read === null) return true;
        boxes.push(...read);
      }
      if (broken(boxes)) return true;
    }
  }
  return false;
}

interface State {
  /** Inside a graphic mark, an axis or decor: shapes are accounted for. */
  covered: boolean;
  /** The nearest axis group. */
  axis: Element | null;
  /** Inside a text mark: its text is compared as a whole. */
  textMark: boolean;
  /** Inside <style> or <script>: text that is not shown. */
  hidden: boolean;
  /** Inside an SVG element that is never drawn where it stands (UNRENDERED). */
  unrendered: boolean;
  /** The parent element's tag, for text nodes. */
  tag: string | undefined;
  /** The parent's computed `hyphenate-character` and `text-emphasis-style` (both inherit). */
  hyphenate: string;
  emphasis: string;
}

/**
 * SVG elements whose content is never drawn where it stands, only as a copy where the page uses it
 * (a marker, a pattern, a clip path or mask, a symbol, definitions, gradients and filters): a mark
 * inside one shows nothing where it is, and wherever it is drawn it is not what the check reads.
 */
const UNRENDERED: ReadonlySet<string> = new Set([
  "defs",
  "marker",
  "pattern",
  "clipPath",
  "mask",
  "symbol",
  "linearGradient",
  "radialGradient",
  "filter",
]);

/** SVG elements whose copy shows text: text itself, and a foreignObject's HTML. */
const TEXT_HOLDERS: ReadonlySet<string> = new Set(["text", "tspan", "textPath", "foreignObject"]);

const XLINK_NS = "http://www.w3.org/1999/xlink";

/**
 * The ids a `<use>` reference names in this document: for a bare fragment (`#id`), the id as
 * written and percent-decoded; nothing for an empty reference; and `null` for every other URL.
 * The comparison cannot tell what another URL draws (a document of its own, or this one under the
 * URL the viewer happens to load it from), so the caller refuses it rather than guess; a dashboard
 * has no use for one, since the sandbox refuses the request it would make. The URL parser drops
 * tabs and newlines anywhere and spaces at either end, so they are dropped here too.
 */
export function fragmentIds(href: string): string[] | null {
  const text = href.replace(/[\t\n\r]/g, "").replace(/^[\0- ]+|[\0- ]+$/g, "");
  if (text === "") return [];
  if (!text.startsWith("#")) return null;
  const raw = text.slice(1);
  const ids = [raw];
  try {
    const decoded = decodeURIComponent(raw);
    if (decoded !== raw) ids.push(decoded);
  } catch {
    // Not valid percent-encoding: the id is only what is written.
  }
  return ids;
}

/**
 * The elements a `<use>` must not draw a copy of: every element that is SVG text, a foreignObject
 * or a mark, every element inside a mark, and every element that holds one of them. A use's copy
 * lives in a shadow tree the comparison does not read, so a copy of text would show it again,
 * turned, clipped or placed anywhere, unchecked.
 */
function uncopyable(dom: DomAccess, root: Element): Set<Element> {
  const refused = new Set<Element>();
  /** Refuses an element and every ancestor up to the first one already refused (whose ancestors are too). */
  const refuse = (element: Element) => {
    for (let node: Node | null = element; node !== null && dom.nodeType(node) === 1; node = dom.parentNode(node)) {
      if (refused.has(node as Element)) return;
      refused.add(node as Element);
    }
  };
  const stack: { element: Element; inMark: boolean }[] = [{ element: root, inMark: false }];
  while (stack.length > 0) {
    const entry = stack.pop();
    if (entry === undefined) break;
    const { element, inMark } = entry;
    const mark = dom.getAttribute(element, "data-tq") !== null;
    const text = dom.namespaceURI(element) === SVG_NS && TEXT_HOLDERS.has(dom.localName(element));
    if (inMark || mark || text) refuse(element);
    for (let child = dom.firstChild(element); child !== null; child = dom.nextSibling(child)) {
      if (dom.nodeType(child) === 1) stack.push({ element: child as Element, inMark: inMark || mark });
    }
  }
  return refused;
}

/** The scroll buttons a scroll container draws with `::scroll-button()`, one per direction. */
const SCROLL_BUTTONS = [
  "::scroll-button(up)",
  "::scroll-button(down)",
  "::scroll-button(left)",
  "::scroll-button(right)",
  "::scroll-button(block-start)",
  "::scroll-button(block-end)",
  "::scroll-button(inline-start)",
  "::scroll-button(inline-end)",
] as const;

interface GeometryMark {
  element: Element;
  reference: string;
  resolved: Resolved;
}

interface AxisTick {
  /** The tick's text mark. */
  element: Element;
  reference: string;
  value: number;
}

/**
 * Judges the document against the data: every rule of the README's table except those only the
 * checker sees (script errors, navigation, requests, closed shadow roots, the time limits).
 *
 * `layout` is the caller's fact, never read from the page: whether the document is laid out by a
 * browser (the checker and the runtime pass true; a unit test's DOM, which reports every box as
 * empty, passes false). The page styles its own root, so the root's box must not decide whether
 * the layout rules run.
 */
export function compareDocument(dom: DomAccess, model: Model, locale: Locale, layout: boolean): Comparison {
  const findings: Finding[] = [];
  const marks: MarkReport[] = [];
  const reported: Record<string, true> = Object.create(null);
  const groups: Record<string, GeometryMark[]> = Object.create(null);
  const groupOrder: string[] = [];
  const axes: { element: Element; ticks: AxisTick[] }[] = [];
  const ranges: Record<string, readonly [number, number] | null> = Object.create(null);
  const root = dom.documentElement();
  if (root === null) return { findings: [{ problem: { kind: "not_rendered" }, fix: null }], marks };
  // Counted first: a document past a limit is judged by that alone, before the costlier rules
  // (computed styles, layout) run over it.
  const limit = sizeLimit(dom, root);
  if (limit !== null) return { findings: [{ problem: limit, fix: null }], marks };
  // With layout, the root always has an area: the frame's width, and #tq-root's padding in the
  // sandbox's own styles. A root box with no area means the page hid how its marks render (the
  // document's own box emptied while its content overflows it, or a frame that is not laid out),
  // so every mark is a finding.
  const rootBox = layout ? dom.box(root) : null;
  const unlaid = rootBox !== null && (rootBox.width <= 0 || rootBox.height <= 0);
  const present = presenter(dom, layout);
  /** What a <use> must not copy (uncopyable), read on the first <use>. */
  let copyRefused: Set<Element> | null = null;

  const add = (problem: Problem, fix: Fix | null) => {
    findings.push({ problem, fix });
  };
  const tagOf = (element: Element): string | undefined => {
    const name = dom.localName(element);
    return TAGS.has(name) ? name : undefined;
  };
  const withTag = <T extends object>(problem: T, tag: string | undefined): T =>
    tag === undefined ? problem : { ...problem, tag };

  /** The extent a continuous axis over a column may cover; `null` for a column that has none. */
  const axisRange = (dataset: string, column: string, time: boolean): readonly [number, number] | null => {
    const key = `${dataset}|${column}|${time}`;
    if (key in ranges) return ranges[key] ?? null;
    const result = resultOf(model, dataset);
    const type = result === undefined ? undefined : columnType(result, column);
    const values = result === undefined ? null : columnValues(result, column);
    let range: readonly [number, number] | null = null;
    if (values !== null && type === "number" && !time) {
      range = numberAxisRange(values.filter((value): value is number => typeof value === "number"));
    } else if (values !== null && type === "time" && time) {
      const times: number[] = [];
      for (const value of values) {
        const date = typeof value === "string" ? parseWallTime(value) : null;
        if (date !== null) times.push(date.getTime());
      }
      range = timeAxisRange(times);
    }
    ranges[key] = range;
    return range;
  };

  const markElement = (
    element: Element,
    state: State,
    tag: string | undefined,
    unrendered: boolean,
  ): { graphic: boolean } => {
    const text = dom.getAttribute(element, "data-tq") ?? "";
    const chart = dom.getAttribute(element, "data-tq-chart");
    const channel = dom.getAttribute(element, "data-tq-channel");
    const graphic = chart !== null || channel !== null;
    const fix: Fix = graphic ? { kind: "graphic", element } : { kind: "text", element };
    const reference = parseReference(text);
    if (reference === null) {
      add(withTag({ kind: "unmapped" }, tag), fix);
      return { graphic };
    }
    const canonical = formatReference(reference);
    const unmapped = () =>
      add(
        withTag(declares(model, reference) ? { kind: "unmapped", reference: canonical } : { kind: "unmapped" }, tag),
        fix,
      );
    // A tick reference only marks a tick of a tq.axis axis: anywhere else it could show any value.
    if (reference.form === "t" && (graphic || state.axis === null)) {
      unmapped();
      return { graphic };
    }
    const resolved = resolve(model, reference);
    if (resolved === null) {
      unmapped();
      return { graphic };
    }
    if (graphic) {
      const knownChannel = MARK_CHANNELS.find((candidate) => candidate === channel);
      if (chart === null || !NAME_PATTERN.test(chart) || knownChannel === undefined) {
        unmapped();
        return { graphic };
      }
      if (unlaid || unrendered) {
        add({ kind: "mark_geometry", reference: canonical, channel: knownChannel }, fix);
      } else {
        const key = `${chart}|${knownChannel}`;
        if (!(key in groups)) {
          groups[key] = [];
          groupOrder.push(key);
        }
        groups[key]?.push({ element, reference: canonical, resolved });
      }
    } else {
      const format = dom.getAttribute(element, "data-tq-format") ?? undefined;
      let expected: string | null;
      try {
        expected = formatValue(resolved.display, format, locale);
      } catch (error) {
        if (!(error instanceof FormatError)) throw error;
        expected = null;
      }
      if (
        expected === null ||
        dom.textContent(element) !== expected ||
        unlaid ||
        unrendered ||
        concealsText(dom, element, layout, present)
      ) {
        add(withTag({ kind: "mismatch", reference: canonical, value: resolved.value }, tag), fix);
      } else if (reference.form === "t" && state.axis !== null) {
        tick(element, reference, canonical, state.axis);
      }
    }
    if (!(canonical in reported)) {
      reported[canonical] = true;
      marks.push({ reference: canonical, value: resolved.value });
    }
    return { graphic };
  };

  const tick = (element: Element, reference: Extract<Reference, { form: "t" }>, canonical: string, axis: Element) => {
    const time = typeof reference.value === "string";
    const value = typeof reference.value === "number" ? reference.value : tickTimeDate(reference.value).getTime();
    const range = axisRange(reference.dataset, reference.column, time);
    if (range === null || !within(value, range)) {
      add({ kind: "mark_geometry", reference: canonical, channel: "axis" }, { kind: "text", element });
      return;
    }
    let group = axes.find((candidate) => candidate.element === axis);
    if (group === undefined) {
      group = { element: axis, ticks: [] };
      axes.push(group);
    }
    group.ticks.push({ element, reference: canonical, value });
  };

  /** The `content` of one of the element's pseudo-elements: numbers in its text, or content it cannot read. */
  const generated = (element: Element, pseudo: string, tag: string | undefined) => {
    const content = parseCssContent(dom.style(element, pseudo, "content"));
    if (content.other) {
      add({ kind: "unsupported_content", content: "generated_content" }, { kind: "pseudo", element });
      return;
    }
    const strings = content.quotes
      ? [...content.strings, ...parseCssContent(dom.style(element, pseudo, "quotes")).strings]
      : content.strings;
    for (const text of strings) {
      const literal = literalOf(text);
      if (literal !== null) {
        add(withTag({ kind: "literal_number", place: "css-content", literal }, tag), { kind: "pseudo", element });
        break;
      }
    }
  };

  const pseudoContent = (element: Element, tag: string | undefined, ordinal: number) => {
    for (const pseudo of PSEUDO_ELEMENTS) generated(element, pseudo, tag);
    // A scroll container draws scroll buttons with their own content.
    const scrolls = (axis: string) => {
      const overflow = dom.style(element, null, axis);
      return overflow === "hidden" || overflow === "scroll" || overflow === "auto";
    };
    if (scrolls("overflow-x") || scrolls("overflow-y")) {
      for (const pseudo of SCROLL_BUTTONS) generated(element, pseudo, tag);
    }
    // Scroll markers are drawn for a group of them, from content the check does not read: any group
    // is refused, whatever its markers say.
    const markers = dom.style(element, null, "scroll-marker-group").trim();
    if (markers !== "" && markers !== "none") {
      add({ kind: "unsupported_content", content: "generated_content" }, { kind: "pseudo", element });
    }
    if (dom.style(element, null, "display").includes("list-item")) {
      const marker = dom.style(element, null, "list-style-type").trim();
      let literal: string | null = null;
      if (marker.startsWith('"') || marker.startsWith("'")) {
        for (const text of parseCssContent(marker).strings) literal ??= literalOf(text);
      } else if (marker !== "" && !SYMBOL_MARKERS.has(marker)) {
        literal = String(ordinal);
      }
      if (literal !== null) {
        add(withTag({ kind: "literal_number", place: "list-marker", literal }, tag), { kind: "pseudo", element });
      }
    }
  };

  const visitElement = (element: Element, state: State, ordinal: number): State => {
    const name = dom.localName(element);
    const namespace = dom.namespaceURI(element);
    const tag = tagOf(element);
    const svg = namespace === SVG_NS;
    // feImage draws a picture, or a copy of another element, into a filter's result.
    const unsupported =
      namespace === HTML_NS
        ? UNSUPPORTED_HTML[name]
        : svg && (name === "image" || name === "feImage")
          ? "image"
          : undefined;
    if (unsupported !== undefined)
      add({ kind: "unsupported_content", content: unsupported }, { kind: "hide", element });
    if (dom.shadowRoot(element) !== null) {
      add({ kind: "unsupported_content", content: "shadow_root" }, { kind: "hide", element });
    }
    // A marquee moves its content for as long as it is shown, with no animation the DOM reports.
    if ((svg && SMIL.has(name)) || (namespace === HTML_NS && name === "marquee")) {
      add({ kind: "timeout", phase: "render" }, null);
    }
    for (const place of TEXT_ATTRIBUTES) {
      const value = dom.getAttribute(element, place);
      const literal = value === null ? null : literalOf(value);
      if (literal !== null) {
        add(withTag({ kind: "literal_number", place, literal }, tag), { kind: "attribute", element, name: place });
      }
    }
    pseudoContent(element, tag, ordinal);
    // Text drawn by inherited styles: the hyphen of a broken word and the emphasis marks over each
    // character. Judged where the value changes from the parent's.
    const hyphenate = dom.style(element, null, "hyphenate-character").trim();
    const emphasis = dom.style(element, null, "text-emphasis-style").trim();
    for (const [value, inherited] of [
      [hyphenate, state.hyphenate],
      [emphasis, state.emphasis],
    ] as const) {
      if (value === inherited) continue;
      let literal: string | null = null;
      for (const text of parseCssContent(value).strings) literal ??= literalOf(text);
      if (literal !== null) {
        add(withTag({ kind: "literal_number", place: "css-content", literal }, tag), { kind: "pseudo", element });
      }
    }
    // A <use> draws a copy of another element in a shadow tree the comparison does not read: a
    // copy of text or of a mark, wherever the use sits (decor too), shows it unchecked. A
    // reference that is not a bare fragment of this document cannot be read, so it is refused too.
    if (svg && name === "use") {
      copyRefused ??= uncopyable(dom, root);
      const refused = copyRefused;
      let copiesText = false;
      for (const href of [dom.getAttributeNS(element, null, "href"), dom.getAttributeNS(element, XLINK_NS, "href")]) {
        if (href === null) continue;
        const ids = fragmentIds(href);
        if (ids === null) {
          copiesText = true;
          continue;
        }
        for (const id of ids) {
          const target = dom.getElementById(id);
          if (target !== null && refused.has(target)) copiesText = true;
        }
      }
      if (copiesText) add(withTag({ kind: "unmapped" }, tag), { kind: "hide", element });
    }

    const next: State = { ...state, tag, hyphenate, emphasis };
    if (svg && UNRENDERED.has(name)) next.unrendered = true;
    if (name === "style" || name === "script") next.hidden = true;
    if (dom.getAttribute(element, "data-tq-axis") !== null) {
      next.axis = element;
      next.covered = true;
    }
    if (dom.getAttribute(element, "data-tq-decor") !== null) next.covered = true;
    if (dom.getAttribute(element, "data-tq") !== null) {
      const { graphic } = markElement(element, state, tag, next.unrendered);
      if (graphic) next.covered = true;
      else next.textMark = true;
    }
    if (svg && SHAPES.has(name) && !next.covered) add(withTag({ kind: "unmapped" }, tag), { kind: "graphic", element });
    return next;
  };

  const initial: State = {
    covered: false,
    axis: null,
    textMark: false,
    hidden: false,
    unrendered: false,
    tag: undefined,
    hyphenate: "",
    emphasis: "",
  };
  const stack: { node: Node; state: State; ordinal: number }[] = [{ node: root, state: initial, ordinal: 1 }];
  while (stack.length > 0) {
    const entry = stack.pop();
    if (entry === undefined) break;
    const { node, state } = entry;
    const type = dom.nodeType(node);
    if (type === 3 || type === 4) {
      if (!state.hidden && !state.textMark) {
        const literal = literalOf(dom.data(node));
        if (literal !== null) {
          const parent = dom.parentNode(node);
          add(withTag({ kind: "literal_number", place: "text", literal }, state.tag), {
            kind: "data",
            node,
            element: parent !== null && dom.nodeType(parent) === 1 ? (parent as Element) : null,
          });
        }
      }
      continue;
    }
    if (type !== 1) continue;
    const next = visitElement(node as Element, state, entry.ordinal);
    const children: Node[] = [];
    for (let child = dom.firstChild(node); child !== null; child = dom.nextSibling(child)) children.push(child);
    let ordinal = children.filter((child) => dom.nodeType(child) === 1).length;
    for (let i = children.length - 1; i >= 0; i--) {
      const child = children[i];
      if (child === undefined) continue;
      const element = dom.nodeType(child) === 1;
      stack.push({ node: child, state: next, ordinal: element ? ordinal : 0 });
      if (element) ordinal--;
    }
  }

  // A custom highlight restyles any range of the document's text, which no element's style shows.
  if (dom.highlights() > 0)
    add({ kind: "unsupported_content", content: "generated_content" }, { kind: "hide", element: root });
  if (dom.liveAnimations() > 0) add({ kind: "timeout", phase: "render" }, null);
  for (const key of groupOrder) geometry(dom, key, groups[key] ?? [], add, present);
  for (const axis of axes) {
    const placed = axis.ticks.map((tick) => ({ tick, at: tickPosition(dom, tick.element) }));
    const xs: Placed[] = placed.map(({ tick, at }) => ({ value: tick.value, position: at.x }));
    const ys: Placed[] = placed.map(({ tick, at }) => ({ value: tick.value, position: at.y }));
    const spread = (points: Placed[]) =>
      points.reduce((high, point) => Math.max(high, point.position), Number.NEGATIVE_INFINITY) -
      points.reduce((low, point) => Math.min(low, point.position), Number.POSITIVE_INFINITY);
    // The axis runs along the coordinate its ticks spread over.
    const along = spread(xs) >= spread(ys) ? xs : ys;
    const off = offLine(along);
    for (let i = 0; i < off.length; i++) {
      const tick = axis.ticks[i];
      if (off[i] && tick !== undefined) {
        add(
          { kind: "mark_geometry", reference: tick.reference, channel: "axis" },
          { kind: "text", element: tick.element },
        );
      }
    }
  }
  return { findings, marks };
}

/** `too_large` when the document has more elements or marks than the limits, else `null`. */
function sizeLimit(dom: DomAccess, root: Element): Problem | null {
  let elements = 0;
  let marks = 0;
  const stack: Node[] = [root];
  while (stack.length > 0) {
    const node = stack.pop();
    if (node === undefined || dom.nodeType(node) !== 1) continue;
    elements++;
    if (elements > MAX_ELEMENTS) return { kind: "too_large", limit: "elements", max: MAX_ELEMENTS };
    if (dom.getAttribute(node as Element, "data-tq") !== null) {
      marks++;
      if (marks > MAX_MARKS) return { kind: "too_large", limit: "marks", max: MAX_MARKS };
    }
    for (let child = dom.firstChild(node); child !== null; child = dom.nextSibling(child)) stack.push(child);
  }
  return null;
}

/** Where a tick sits: the origin of its tick group (d3's `g.tick`), else the middle of its text. */
function tickPosition(dom: DomAccess, element: Element): { x: number; y: number } {
  const parent = dom.parentNode(element);
  if (parent !== null && dom.nodeType(parent) === 1) {
    const group = parent as Element;
    if (dom.namespaceURI(group) === SVG_NS && dom.localName(group) === "g") {
      const origin = dom.origin(group);
      if (origin !== null) return origin;
    }
  }
  const box = dom.box(element);
  return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
}

function numeric(display: Resolved["display"]): number | null | undefined {
  if (display === null) return null;
  if (typeof display === "number") return display;
  if (display instanceof Date) return display.getTime();
  return undefined;
}

/** The rules of one chart's channel: sizes proportional to values, positions linear in them. */
function geometry(
  dom: DomAccess,
  key: string,
  marks: GeometryMark[],
  add: (problem: Problem, fix: Fix | null) => void,
  present: (element: Element) => Presentation,
) {
  const channel = key.slice(key.indexOf("|") + 1) as MarkChannel;
  const fail = (mark: GeometryMark) =>
    add({ kind: "mark_geometry", reference: mark.reference, channel }, { kind: "graphic", element: mark.element });
  if (channel === "x" || channel === "y") {
    const placed: { mark: GeometryMark; point: Placed }[] = [];
    for (const mark of marks) {
      const value = numeric(mark.resolved.display);
      if (value === undefined) {
        fail(mark);
      } else if (value !== null) {
        const box = dom.box(mark.element);
        const position = channel === "x" ? box.left + box.width / 2 : box.top + box.height / 2;
        placed.push({ mark, point: { value, position } });
      }
    }
    const off = offLine(placed.map((entry) => entry.point));
    placed.forEach((entry, i) => {
      if (off[i]) fail(entry.mark);
    });
    return;
  }
  const sized: { mark: GeometryMark; value: number; size: number; unit: number }[] = [];
  for (const mark of marks) {
    const display = mark.resolved.display;
    if (display !== null && typeof display !== "number") {
      fail(mark);
      continue;
    }
    const value = display ?? 0;
    if (channel === "angle") {
      const d = dom.style(mark.element, null, "d");
      const path = d.startsWith("path(") ? parseCssContent(d.slice(5)).strings[0] : undefined;
      const slice = sliceAngle(path ?? dom.getAttribute(mark.element, "d") ?? "");
      // A slice's angle is read from its path, before any transform: a chart scaled more in one
      // direction than another shows other angles on screen.
      const linear = present(mark.element).linear;
      if (
        dom.localName(mark.element) !== "path" ||
        slice === null ||
        slice.radius <= 0 ||
        linear === null ||
        !(stretchOf(linear) <= MAX_SLICE_STRETCH)
      ) {
        fail(mark);
        continue;
      }
      sized.push({ mark, value, size: slice.angle, unit: 1 / slice.radius });
    } else {
      const box = dom.box(mark.element);
      const size = channel === "width" ? box.width : channel === "height" ? box.height : box.width / 2;
      sized.push({ mark, value, size, unit: 1 });
    }
  }
  const off = disproportionate(sized);
  sized.forEach((entry, i) => {
    if (off[i]) fail(entry.mark);
  });
}

/** The verdict of a comparison: passed with its marks, or failed with the first 20 distinct problems. */
export function verdictOf(comparison: Comparison): CheckVerdict {
  if (comparison.findings.length === 0) return { verdict: "passed", report: { marks: comparison.marks } };
  const problems: Problem[] = [];
  const seen: Record<string, true> = Object.create(null);
  for (const finding of comparison.findings) {
    const key = JSON.stringify(finding.problem);
    if (key in seen) continue;
    seen[key] = true;
    problems.push(finding.problem);
    if (problems.length === MAX_PROBLEMS) break;
  }
  return { verdict: "failed", problems: problems as [Problem, ...Problem[]] };
}
