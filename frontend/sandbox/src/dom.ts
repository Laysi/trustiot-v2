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

export interface DomAccess {
  documentElement(): Element | null;
  firstChild(node: Node): Node | null;
  nextSibling(node: Node): Node | null;
  parentNode(node: Node): Node | null;
  nodeType(node: Node): number;
  localName(element: Element): string;
  namespaceURI(element: Element): string | null;
  getAttribute(element: Element, name: string): string | null;
  textContent(node: Node): string;
  /** The text of a text node. */
  data(node: Node): string;
  shadowRoot(element: Element): ShadowRoot | null;
  /** The element's border box in the viewport. */
  box(element: Element): Box;
  /** Where an SVG element's own coordinate system starts in the viewport; `null` when unknown. */
  origin(element: Element): Point | null;
  /** A computed style property of the element or one of its pseudo-elements (`"::before"`). */
  style(element: Element, pseudo: string | null, property: string): string;
  /**
   * How many animations (CSS, transitions, Web Animations) can still change the page: running,
   * paused, or waiting to start (`pending`). Finished and idle ones do not count.
   */
  liveAnimations(): number;
  setTextContent(node: Node, text: string): void;
  setData(node: Node, text: string): void;
  setAttribute(element: Element, name: string, value: string): void;
  removeAttribute(element: Element, name: string): void;
}

type AnyFunction = (...args: never[]) => unknown;

/** The window a DomAccess is captured from: the page's, the isolated world's, or a test DOM's. */
export type DomWindow = Window & typeof globalThis;

const apply = Reflect.apply;
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
  const setAttribute = method(element, "setAttribute");
  const removeAttribute = method(element, "removeAttribute");
  const getBoundingClientRect = method(element, "getBoundingClientRect");
  const getComputedStyle = win.getComputedStyle;
  const getPropertyValue = method(win.CSSStyleDeclaration.prototype, "getPropertyValue");
  const getAnimations = method(getPrototypeOf(doc) as object, "getAnimations");
  const playState = win.Animation === undefined ? undefined : getter<string>(win.Animation.prototype, "playState");
  const pending = win.Animation === undefined ? undefined : getter<boolean>(win.Animation.prototype, "pending");
  const graphics = (win as unknown as { SVGGraphicsElement?: { prototype: object } }).SVGGraphicsElement;
  const getScreenCTM = graphics === undefined ? undefined : method(graphics.prototype, "getScreenCTM");
  const rectNumber = (rect: unknown, name: string) => {
    const value = (rect as Record<string, unknown> | null)?.[name];
    return typeof value === "number" && Number.isFinite(value) ? value : 0;
  };
  if (getAttribute === undefined || setAttribute === undefined || removeAttribute === undefined) {
    throw new Error("the DOM has no attribute functions");
  }

  const box = (target: Element): Box => {
    if (getBoundingClientRect === undefined) return { left: 0, top: 0, width: 0, height: 0 };
    const rect = apply(getBoundingClientRect, target, []);
    return {
      left: rectNumber(rect, "left"),
      top: rectNumber(rect, "top"),
      width: rectNumber(rect, "width"),
      height: rectNumber(rect, "height"),
    };
  };

  return {
    documentElement: () => documentElement(doc),
    firstChild: (target) => firstChild(target),
    nextSibling: (target) => nextSibling(target),
    parentNode: (target) => parentNode(target),
    nodeType: (target) => nodeType(target),
    localName: (target) => localName(target),
    namespaceURI: (target) => namespaceURI(target),
    getAttribute: (target, name) => apply(getAttribute, target, [name]) as string | null,
    textContent: (target) => textContentGet[kind(target)](target) ?? "",
    data: (target) => dataGet[textKind(target)](target),
    shadowRoot: (target) => shadowRoot(target) ?? null,
    box,
    origin: (target) => {
      if (getScreenCTM === undefined) return null;
      // The element's user-space origin in client coordinates: its own and its ancestors'
      // transforms, and where its <svg> sits.
      const matrix = apply(getScreenCTM, target, []) as { e?: unknown; f?: unknown } | null;
      if (matrix === null || typeof matrix.e !== "number" || typeof matrix.f !== "number") return null;
      return { x: matrix.e, y: matrix.f };
    },
    style: (target, pseudo, property) => {
      if (getPropertyValue === undefined) return "";
      const declaration = apply(getComputedStyle, win, pseudo === null ? [target] : [target, pseudo]);
      const value = apply(getPropertyValue, declaration, [property]);
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
    setTextContent: (target, text) => textContentSet[kind(target)](target, text),
    setData: (target, text) => dataSet[textKind(target)](target, text),
    setAttribute: (target, name, value) => void apply(setAttribute, target, [name, value]),
    removeAttribute: (target, name) => void apply(removeAttribute, target, [name]),
  };
}
