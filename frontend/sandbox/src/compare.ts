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
import type { DomAccess } from "./dom.ts";
import { FormatError, formatValue } from "./format.ts";
import {
  disproportionate,
  numberAxisRange,
  offLine,
  type Placed,
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

/** Whether a computed colour paints nothing: `transparent`, or an `rgba(...)` with zero alpha. */
export function isTransparent(color: string): boolean {
  const value = color.trim().toLowerCase();
  if (value === "transparent") return true;
  const rgba = /^rgba?\(\s*[\d.]+\s*,\s*[\d.]+\s*,\s*[\d.]+\s*,\s*([\d.]+)\s*\)$/.exec(value);
  return rgba !== null && Number(rgba[1]) === 0;
}

/**
 * Whether a text mark's rendered text may differ from its `textContent`, so comparing the two is
 * not enough (frontend/sandbox/README.md, 「檢查規則」). A text mark is one text node, no element
 * children, shown whole and in order. Flags: an element child (its own text or styling changes the
 * number), a hidden mark (`display: none`, `visibility: hidden`, a transparent colour), a bidi
 * override (the digits read in another order), a restyled `::first-letter`/`::first-line` (a digit
 * hidden, resized or made transparent), a zero-size box (`font-size: 0`), and a clipping box the
 * content overflows, so part is cut off (an ellipsis is this with `text-overflow` set). The
 * box-size and clipping checks need layout, so they run only where there is layout (the checker's
 * browser and the view, not a unit test's DOM), gated by `layoutAvailable`. A mark with no box but
 * visible text (`display: contents`) is not hidden and is not flagged.
 */
export function concealsText(dom: DomAccess, element: Element, layoutAvailable: boolean): boolean {
  for (let child = dom.firstChild(element); child !== null; child = dom.nextSibling(child)) {
    if (dom.nodeType(child) === 1) return true;
  }
  const display = dom.style(element, null, "display");
  if (display === "none") return true;
  const visibility = dom.style(element, null, "visibility");
  if (visibility === "hidden" || visibility === "collapse") return true;
  if (isTransparent(dom.style(element, null, "color"))) return true;
  const bidi = dom.style(element, null, "unicode-bidi");
  if (bidi.includes("override") || bidi === "plaintext") return true;
  const own = dom.style(element, null, "font-size");
  for (const pseudo of ["::first-letter", "::first-line"] as const) {
    const size = dom.style(element, pseudo, "font-size");
    if (size !== "" && own !== "" && size !== own) return true;
    const pseudoVisibility = dom.style(element, pseudo, "visibility");
    if (pseudoVisibility === "hidden" || pseudoVisibility === "collapse") return true;
    if (isTransparent(dom.style(element, pseudo, "color"))) return true;
  }
  if (layoutAvailable) {
    // A box with no area shows nothing (font-size: 0); display: contents has no box of its own but
    // still shows its text through its parent, so it is not hidden.
    if (display !== "contents") {
      const box = dom.box(element);
      if (box.width <= 0 || box.height <= 0) return true;
    }
    const scroll = dom.scrollSize(element);
    if (CLIPPING.has(dom.style(element, null, "overflow-x")) && scroll.scrollWidth - scroll.clientWidth > 1) {
      return true;
    }
    if (CLIPPING.has(dom.style(element, null, "overflow-y")) && scroll.scrollHeight - scroll.clientHeight > 1) {
      return true;
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
  /** The parent element's tag, for text nodes. */
  tag: string | undefined;
}

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
 */
export function compareDocument(dom: DomAccess, model: Model, locale: Locale): Comparison {
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
  // Layout-dependent checks (a mark's box, clipping) run only where there is layout: the checker's
  // browser and the view, not a unit test's DOM, which reports every box as empty.
  const rootBox = dom.box(root);
  const layoutAvailable = rootBox.width > 0 || rootBox.height > 0;

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

  const markElement = (element: Element, state: State, tag: string | undefined): { graphic: boolean } => {
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
      const key = `${chart}|${knownChannel}`;
      if (!(key in groups)) {
        groups[key] = [];
        groupOrder.push(key);
      }
      groups[key]?.push({ element, reference: canonical, resolved });
    } else {
      const format = dom.getAttribute(element, "data-tq-format") ?? undefined;
      let expected: string | null;
      try {
        expected = formatValue(resolved.display, format, locale);
      } catch (error) {
        if (!(error instanceof FormatError)) throw error;
        expected = null;
      }
      if (expected === null || dom.textContent(element) !== expected || concealsText(dom, element, layoutAvailable)) {
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

  const pseudoContent = (element: Element, tag: string | undefined, ordinal: number) => {
    for (const pseudo of PSEUDO_ELEMENTS) {
      const content = parseCssContent(dom.style(element, pseudo, "content"));
      if (content.other) {
        add({ kind: "unsupported_content", content: "generated_content" }, { kind: "pseudo", element });
        continue;
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
    const unsupported = namespace === HTML_NS ? UNSUPPORTED_HTML[name] : svg && name === "image" ? "image" : undefined;
    if (unsupported !== undefined)
      add({ kind: "unsupported_content", content: unsupported }, { kind: "hide", element });
    if (dom.shadowRoot(element) !== null) {
      add({ kind: "unsupported_content", content: "shadow_root" }, { kind: "hide", element });
    }
    if (svg && SMIL.has(name)) add({ kind: "timeout", phase: "render" }, null);
    for (const place of TEXT_ATTRIBUTES) {
      const value = dom.getAttribute(element, place);
      const literal = value === null ? null : literalOf(value);
      if (literal !== null) {
        add(withTag({ kind: "literal_number", place, literal }, tag), { kind: "attribute", element, name: place });
      }
    }
    pseudoContent(element, tag, ordinal);

    const next: State = { ...state, tag };
    if (name === "style" || name === "script") next.hidden = true;
    if (dom.getAttribute(element, "data-tq-axis") !== null) {
      next.axis = element;
      next.covered = true;
    }
    if (dom.getAttribute(element, "data-tq-decor") !== null) next.covered = true;
    if (dom.getAttribute(element, "data-tq") !== null) {
      const { graphic } = markElement(element, state, tag);
      if (graphic) next.covered = true;
      else next.textMark = true;
    }
    if (svg && SHAPES.has(name) && !next.covered) add(withTag({ kind: "unmapped" }, tag), { kind: "graphic", element });
    return next;
  };

  const initial: State = { covered: false, axis: null, textMark: false, hidden: false, tag: undefined };
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

  if (dom.liveAnimations() > 0) add({ kind: "timeout", phase: "render" }, null);
  for (const key of groupOrder) geometry(dom, key, groups[key] ?? [], add);
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
      if (dom.localName(mark.element) !== "path" || slice === null || slice.radius <= 0) {
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
