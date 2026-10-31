// The DOM functions the comparison reads and writes with, captured once from a window's
// prototypes (frontend/sandbox/README.md, 「比對程式」).
//
// In the checker, compare.js captures them in an isolated world, whose prototypes the page
// cannot reach. At display time the runtime captures them before any page script runs, so a page
// that later replaces `textContent` or `getAttribute` on the prototypes, or shadows them on an
// element, does not change what the comparison reads; the page can still interfere in other ways
// in its own realm, which is why the display-time result only decides what to show.

export interface Box {
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface Point {
  x: number;
  y: number;
}

/** An affine map `matrix(a, b, c, d, e, f)`: x' = a·x + c·y + e, y' = b·x + d·y + f. */
export interface Matrix {
  a: number;
  b: number;
  c: number;
  d: number;
  e: number;
  f: number;
}

export interface DomAccess {
  documentElement(): Element | null;
  firstChild(node: Node): Node | null;
  nextSibling(node: Node): Node | null;
  parentNode(node: Node): Node | null;
  nodeType(node: Node): number;
  localName(element: Element): string;
  namespaceURI(element: Element): string | null;
  getAttribute(element: Element, name: string): string | null;
  /** An attribute in a namespace (`null` for none), whatever its prefix. */
  getAttributeNS(element: Element, namespace: string | null, name: string): string | null;
  /** The document's first element with this id, as an SVG reference resolves `#id`. */
  getElementById(id: string): Element | null;
  textContent(node: Node): string;
  /** The text of a text node. */
  data(node: Node): string;
  shadowRoot(element: Element): ShadowRoot | null;
  /** The element's border box in the viewport. */
  box(element: Element): Box;
  /**
   * Where a text node's glyphs are drawn: one box per fragment of it the layout made (a line, or a
   * run of one direction); empty when unknown, `null` when there are more than MAX_TEXT_BOXES.
   */
  textBoxes(node: Node): Box[] | null;
  /**
   * Whether the element is drawn as far as the browser knows (`checkVisibility()`): false when it
   * or an ancestor is `display: none` or its content is skipped (`content-visibility: hidden`, a
   * closed `<details>`); true when unknown.
   */
  visible(element: Element): boolean;
  /** The element's scrollable content size and its visible (client) size, for clipping. */
  scrollSize(element: Element): {
    scrollWidth: number;
    clientWidth: number;
    scrollHeight: number;
    clientHeight: number;
  };
  /** Where an SVG element's own coordinate system starts in the viewport; `null` when unknown. */
  origin(element: Element): Point | null;
  /**
   * How an SVG element's own coordinates map to the viewport (`getScreenCTM`): its own and every
   * ancestor's transform, HTML ancestors included, and its `<svg>`'s viewBox; `null` when unknown.
   */
  screenMatrix(element: Element): Matrix | null;
  /** A computed style property of the element or one of its pseudo-elements (`"::before"`). */
  style(element: Element, pseudo: string | null, property: string): string;
  /**
   * How many animations (CSS, transitions, Web Animations) can still change the page: running,
   * paused, or waiting to start (`pending`). Finished and idle ones do not count.
   */
  liveAnimations(): number;
  /** How many custom highlights the page registered (`CSS.highlights`); 0 when the DOM has none. */
  highlights(): number;
  setTextContent(node: Node, text: string): void;
  setData(node: Node, text: string): void;
  setAttribute(element: Element, name: string, value: string): void;
  removeAttribute(element: Element, name: string): void;
}

type AnyFunction = (...args: never[]) => unknown;

/** The window a DomAccess is captured from: the page's, the isolated world's, or a test DOM's. */
export type DomWindow = Window & typeof globalThis;

const apply = Reflect.apply;
/** The most fragments of one text node read: a text in more pieces is reported as `null`. */
const MAX_TEXT_BOXES = 64;
const getPrototypeOf = Object.getPrototypeOf;
const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;

function descriptor(prototype: object | undefined, name: string): PropertyDescriptor | undefined {
  for (let current = prototype; current; current = getPrototypeOf(current) as object | undefined) {
    const found = getOwnPropertyDescriptor(current, name);
    if (found !== undefined) return found;
  }
  return undefined;
}

/** The getter of `name` on `prototype`'s chain; a plain read when the DOM defines no getter there. */
function getter<T>(prototype: object | undefined, name: string): (target: object) => T {
  const get = descriptor(prototype, name)?.get;
  if (get !== undefined) return (target) => apply(get, target, []) as T;
  return (target) => (target as Record<string, T>)[name] as T;
}

function setter(prototype: object | undefined, name: string): (target: object, value: unknown) => void {
  const set = descriptor(prototype, name)?.set;
  if (set !== undefined) return (target, value) => void apply(set, target, [value]);
  return (target, value) => {
    (target as Record<string, unknown>)[name] = value;
  };
}

function method(prototype: object | undefined, name: string): AnyFunction | undefined {
  const value = descriptor(prototype, name)?.value;
  return typeof value === "function" ? (value as AnyFunction) : undefined;
}

/** Captures the DOM functions of `win` now; later changes to its prototypes do not reach them. */
export function captureDom(win: DomWindow): DomAccess {
  const node = win.Node.prototype;
  const element = win.Element.prototype;
  const characterData = win.CharacterData.prototype;
  const textNode = win.Text.prototype;
  const doc = win.document;
  const documentElement = getter<Element | null>(getPrototypeOf(doc) as object, "documentElement");
  const firstChild = getter<Node | null>(node, "firstChild");
  const nextSibling = getter<Node | null>(node, "nextSibling");
  const parentNode = getter<Node | null>(node, "parentNode");
  const nodeType = getter<number>(node, "nodeType");
  // Browsers define textContent on Node and data on CharacterData only; a test DOM may override
  // them further down, so each kind of node gets the accessor its own prototype chain has.
  const textContentGet = {
    node: getter<string | null>(node, "textContent"),
    element: getter<string | null>(element, "textContent"),
    characterData: getter<string | null>(characterData, "textContent"),
  };
  const textContentSet = {
    node: setter(node, "textContent"),
    element: setter(element, "textContent"),
    characterData: setter(characterData, "textContent"),
  };
  const kind = (target: Node) => {
    const type = nodeType(target);
    return type === 1 ? "element" : type === 3 || type === 4 || type === 7 || type === 8 ? "characterData" : "node";
  };
  const localName = getter<string>(element, "localName");
  const namespaceURI = getter<string | null>(element, "namespaceURI");
  const shadowRoot = getter<ShadowRoot | null>(element, "shadowRoot");
  const dataGet = { text: getter<string>(textNode, "data"), other: getter<string>(characterData, "data") };
  const dataSet = { text: setter(textNode, "data"), other: setter(characterData, "data") };
  const textKind = (target: Node) => {
    const type = nodeType(target);
    return type === 3 || type === 4 ? "text" : "other";
  };
  const getAttribute = method(element, "getAttribute");
  const getAttributeNS = method(element, "getAttributeNS");
  const getElementById = method(getPrototypeOf(doc) as object, "getElementById");
  const checkVisibility = method(element, "checkVisibility");
  const setAttribute = method(element, "setAttribute");
  const removeAttribute = method(element, "removeAttribute");
  const getBoundingClientRect = method(element, "getBoundingClientRect");
  const scrollWidth = getter<number>(element, "scrollWidth");
  const scrollHeight = getter<number>(element, "scrollHeight");
  const clientWidth = getter<number>(element, "clientWidth");
  const clientHeight = getter<number>(element, "clientHeight");
  const size = (value: number) => (typeof value === "number" && Number.isFinite(value) ? value : 0);
  const getComputedStyle = win.getComputedStyle;
  const getPropertyValue = method(win.CSSStyleDeclaration.prototype, "getPropertyValue");
  const getAnimations = method(getPrototypeOf(doc) as object, "getAnimations");
  const playState = win.Animation === undefined ? undefined : getter<string>(win.Animation.prototype, "playState");
  const pending = win.Animation === undefined ? undefined : getter<boolean>(win.Animation.prototype, "pending");
  const graphics = (win as unknown as { SVGGraphicsElement?: { prototype: object } }).SVGGraphicsElement;
  const getScreenCTM = graphics === undefined ? undefined : method(graphics.prototype, "getScreenCTM");
  // Whether an element is an SVGGraphicsElement, by the brand check of one of that interface's own
  // getters (captured `transform`, which throws on any other element), never by the page's
  // `instanceof` or prototype chain, which a page can rewrite. getScreenCTM throws on the elements
  // that are not (marker, pattern, clipPath, mask, symbol).
  const graphicsTransform = graphics === undefined ? undefined : descriptor(graphics.prototype, "transform")?.get;
  const isGraphics = (target: object) => {
    if (graphicsTransform === undefined) return false;
    try {
      apply(graphicsTransform, target, []);
      return true;
    } catch {
      return false;
    }
  };
  // A text node's fragments, through a Range (captured, like every other function here).
  const createRange = method(getPrototypeOf(doc) as object, "createRange");
  const rangePrototype = (win as unknown as { Range?: { prototype: object } }).Range?.prototype;
  const selectNodeContents = method(rangePrototype, "selectNodeContents");
  const rangeRects = method(rangePrototype, "getClientRects");
  const rectListPrototype = (win as unknown as { DOMRectList?: { prototype: object } }).DOMRectList?.prototype;
  const rectListLength = getter<unknown>(rectListPrototype, "length");
  const rectListItem = method(rectListPrototype, "item");
  // The custom highlight registry, from the CSS namespace's own getter, and its size.
  const cssNamespace = (win as unknown as { CSS?: object }).CSS;
  const highlightsGet = cssNamespace === undefined ? undefined : descriptor(cssNamespace, "highlights")?.get;
  const registryPrototype = (win as unknown as { HighlightRegistry?: { prototype: object } }).HighlightRegistry
    ?.prototype;
  const registrySize = registryPrototype === undefined ? undefined : descriptor(registryPrototype, "size")?.get;
  // The DOMRect and matrix getters too: a page that later replaces `DOMRectReadOnly.prototype`'s
  // `width` or `SVGMatrix.prototype`'s `a` must not change the boxes and matrices read here.
  const rectPrototype = (win as unknown as { DOMRectReadOnly?: { prototype: object } }).DOMRectReadOnly?.prototype;
  const rectGet = {
    left: getter<unknown>(rectPrototype, "left"),
    top: getter<unknown>(rectPrototype, "top"),
    width: getter<unknown>(rectPrototype, "width"),
    height: getter<unknown>(rectPrototype, "height"),
  };
  // Chromium's getScreenCTM returns an SVGMatrix, whose getters are not DOMMatrixReadOnly's (each
  // throws on the other's objects); a DOM without SVGMatrix returns a DOMMatrix. Each matrix is read
  // with the getters of the interface it is an instance of, found through the captured prototypes.
  const matrixInterfaces = win as unknown as {
    SVGMatrix?: { prototype: object };
    DOMMatrix?: { prototype: object };
    DOMMatrixReadOnly?: { prototype: object };
  };
  const matrixReaders: { prototype: object; get: Record<keyof Matrix, (target: object) => unknown> }[] = [];
  for (const prototype of [
    matrixInterfaces.SVGMatrix?.prototype,
    matrixInterfaces.DOMMatrix?.prototype,
    matrixInterfaces.DOMMatrixReadOnly?.prototype,
  ]) {
    if (prototype === undefined) continue;
    matrixReaders.push({
      prototype,
      get: {
        a: getter<unknown>(prototype, "a"),
        b: getter<unknown>(prototype, "b"),
        c: getter<unknown>(prototype, "c"),
        d: getter<unknown>(prototype, "d"),
        e: getter<unknown>(prototype, "e"),
        f: getter<unknown>(prototype, "f"),
      },
    });
  }
  const readerOf = (matrix: object) => {
    for (let current = getPrototypeOf(matrix) as object | null; current !== null; current = getPrototypeOf(current)) {
      for (const reader of matrixReaders) if (reader.prototype === current) return reader;
    }
    return undefined;
  };
  const finite = (value: unknown): number | null =>
    typeof value === "number" && Number.isFinite(value) ? value : null;
  const rectNumber = (rect: object, name: keyof typeof rectGet) => finite(rectGet[name](rect)) ?? 0;
  if (getAttribute === undefined || setAttribute === undefined || removeAttribute === undefined) {
    throw new Error("the DOM has no attribute functions");
  }

  const screenMatrix = (target: Element): Matrix | null => {
    if (getScreenCTM === undefined || !isGraphics(target)) return null;
    let matrix: object | null;
    try {
      matrix = apply(getScreenCTM, target, []) as object | null;
    } catch {
      // Defence in depth: the brand check above already let only graphics elements through.
      return null;
    }
    if (matrix === null || typeof matrix !== "object") return null;
    const reader = readerOf(matrix);
    if (reader === undefined) return null;
    const a = finite(reader.get.a(matrix));
    const b = finite(reader.get.b(matrix));
    const c = finite(reader.get.c(matrix));
    const d = finite(reader.get.d(matrix));
    const e = finite(reader.get.e(matrix));
    const f = finite(reader.get.f(matrix));
    if (a === null || b === null || c === null || d === null || e === null || f === null) return null;
    return { a, b, c, d, e, f };
  };

  const box = (target: Element): Box => {
    if (getBoundingClientRect === undefined) return { left: 0, top: 0, width: 0, height: 0 };
    const rect = apply(getBoundingClientRect, target, []) as object;
    return {
      left: rectNumber(rect, "left"),
      top: rectNumber(rect, "top"),
      width: rectNumber(rect, "width"),
      height: rectNumber(rect, "height"),
    };
  };

  const textBoxes = (target: Node): Box[] | null => {
    if (
      createRange === undefined ||
      selectNodeContents === undefined ||
      rangeRects === undefined ||
      rectListItem === undefined
    ) {
      return [];
    }
    const range = apply(createRange, doc, []) as object;
    apply(selectNodeContents, range, [target]);
    const list = apply(rangeRects, range, []) as object;
    const length = finite(rectListLength(list)) ?? 0;
    if (length > MAX_TEXT_BOXES) return null;
    const boxes: Box[] = [];
    for (let i = 0; i < length; i++) {
      const rect = apply(rectListItem, list, [i]) as object | null;
      if (rect === null || typeof rect !== "object") continue;
      boxes.push({
        left: rectNumber(rect, "left"),
        top: rectNumber(rect, "top"),
        width: rectNumber(rect, "width"),
        height: rectNumber(rect, "height"),
      });
    }
    return boxes;
  };

  // One computed style declaration is read for many properties in a row; it is live, so reusing
  // the last one is the same as asking again.
  let styleTarget: Element | null = null;
  let stylePseudo: string | null = null;
  let styleDeclaration: unknown = null;

  return {
    scrollSize: (target) => ({
      scrollWidth: size(scrollWidth(target)),
      clientWidth: size(clientWidth(target)),
      scrollHeight: size(scrollHeight(target)),
      clientHeight: size(clientHeight(target)),
    }),
    documentElement: () => documentElement(doc),
    firstChild: (target) => firstChild(target),
    nextSibling: (target) => nextSibling(target),
    parentNode: (target) => parentNode(target),
    nodeType: (target) => nodeType(target),
    localName: (target) => localName(target),
    namespaceURI: (target) => namespaceURI(target),
    getAttribute: (target, name) => apply(getAttribute, target, [name]) as string | null,
    getAttributeNS: (target, namespace, name) =>
      getAttributeNS === undefined ? null : (apply(getAttributeNS, target, [namespace, name]) as string | null),
    getElementById: (id) =>
      getElementById === undefined ? null : (apply(getElementById, doc, [id]) as Element | null),
    textContent: (target) => textContentGet[kind(target)](target) ?? "",
    data: (target) => dataGet[textKind(target)](target),
    shadowRoot: (target) => shadowRoot(target) ?? null,
    box,
    textBoxes,
    visible: (target) => checkVisibility === undefined || apply(checkVisibility, target, []) !== false,
    origin: (target) => {
      // The element's user-space origin in client coordinates: its own and its ancestors'
      // transforms, and where its <svg> sits.
      const matrix = screenMatrix(target);
      return matrix === null ? null : { x: matrix.e, y: matrix.f };
    },
    screenMatrix,
    style: (target, pseudo, property) => {
      if (getPropertyValue === undefined) return "";
      if (styleDeclaration === null || styleTarget !== target || stylePseudo !== pseudo) {
        styleDeclaration = apply(getComputedStyle, win, pseudo === null ? [target] : [target, pseudo]);
        styleTarget = target;
        stylePseudo = pseudo;
      }
      const value = apply(getPropertyValue, styleDeclaration, [property]);
      return typeof value === "string" ? value : "";
    },
    liveAnimations: () => {
      if (getAnimations === undefined || playState === undefined) return 0;
      const animations = apply(getAnimations, doc, []) as ArrayLike<object>;
      let live = 0;
      for (let i = 0; i < animations.length; i++) {
        const animation = animations[i];
        if (animation === undefined) continue;
        const state = playState(animation);
        if (state === "running" || state === "paused" || (pending !== undefined && pending(animation) === true)) live++;
      }
      return live;
    },
    highlights: () => {
      if (highlightsGet === undefined || registrySize === undefined) return 0;
      const registry = apply(highlightsGet, cssNamespace, []) as object | null;
      if (registry === null || typeof registry !== "object") return 0;
      return finite(apply(registrySize, registry, [])) ?? 0;
    },
    setTextContent: (target, text) => textContentSet[kind(target)](target, text),
    setData: (target, text) => dataSet[textKind(target)](target, text),
    setAttribute: (target, name, value) => void apply(setAttribute, target, [name, value]),
    removeAttribute: (target, name) => void apply(removeAttribute, target, [name]),
  };
}
