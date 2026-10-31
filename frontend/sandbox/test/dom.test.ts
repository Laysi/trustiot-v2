// The DOM functions the comparison captures before the page runs (src/dom.ts).
import { afterEach, describe, expect, test } from "bun:test";

import { Window } from "happy-dom";

import { captureDom, type DomWindow } from "../src/dom.ts";
import { guardPrototypes } from "./harness.ts";

let cleanup: (() => Promise<void>) | null = null;

afterEach(async () => {
  await cleanup?.();
  cleanup = null;
});

function openWindow(): DomWindow {
  const happy = new Window({ url: "https://sandbox.example.test/sandbox/v1/sandbox.html" });
  const restore = guardPrototypes(happy);
  cleanup = async () => {
    await happy.happyDOM.close();
    restore();
  };
  return happy as unknown as DomWindow;
}

/** A Web Animations stand-in: happy-dom has no `getAnimations`. */
class FakeAnimation {
  constructor(
    private readonly state: string,
    private readonly waiting: boolean,
  ) {}
  get playState() {
    return this.state;
  }
  get pending() {
    return this.waiting;
  }
}

function withAnimations(win: DomWindow, animations: FakeAnimation[]) {
  Object.defineProperty(win, "Animation", { value: FakeAnimation, configurable: true });
  // A prototype of this document's own, so the shared Document prototype stays as it is.
  const own = Object.create(Object.getPrototypeOf(win.document)) as object;
  Object.defineProperty(own, "getAnimations", { value: () => animations, configurable: true });
  Object.setPrototypeOf(win.document, own);
}

describe("live animations", () => {
  test("running, paused and pending ones count; finished and idle ones do not", () => {
    const win = openWindow();
    withAnimations(win, [
      new FakeAnimation("running", false),
      new FakeAnimation("paused", false),
      new FakeAnimation("idle", true),
      new FakeAnimation("finished", false),
      new FakeAnimation("idle", false),
    ]);
    expect(captureDom(win).liveAnimations()).toBe(3);
  });

  test("the page changing the getters later does not change the count", () => {
    const win = openWindow();
    withAnimations(win, [new FakeAnimation("running", false)]);
    const dom = captureDom(win);
    const restore = guardPrototypes({ FakeAnimation });
    Object.defineProperty(FakeAnimation.prototype, "playState", { get: () => "finished", configurable: true });
    Object.defineProperty(FakeAnimation.prototype, "pending", { get: () => false, configurable: true });
    try {
      expect(dom.liveAnimations()).toBe(1);
    } finally {
      restore();
    }
  });

  test("a DOM without Web Animations has none", () => {
    expect(captureDom(openWindow()).liveAnimations()).toBe(0);
  });
});

describe("captured functions", () => {
  test("reads go through the functions saved at capture, not the page's replacements", () => {
    const win = openWindow();
    const dom = captureDom(win);
    const p = win.document.createElement("p");
    p.textContent = "42";
    win.document.body.appendChild(p);
    Object.defineProperty(win.Element.prototype, "getAttribute", { value: () => "forged", configurable: true });
    Object.defineProperty(win.Node.prototype, "textContent", { get: () => "", configurable: true });
    Object.defineProperty(win.Element.prototype, "textContent", { get: () => "", configurable: true });
    p.setAttribute("data-tq", "f|week_kwh");
    expect(dom.getAttribute(p, "data-tq")).toBe("f|week_kwh");
    expect(dom.textContent(p)).toBe("42");
    expect(dom.localName(p)).toBe("p");
  });
});

describe("boxes and screen matrices", () => {
  test("a box is read with the DOMRect getters saved at capture", () => {
    const win = openWindow();
    const dom = captureDom(win);
    const p = win.document.body.appendChild(win.document.createElement("p"));
    const before = dom.box(p);
    for (const prototype of [win.DOMRect.prototype, win.DOMRectReadOnly.prototype]) {
      for (const name of ["left", "top", "width", "height"]) {
        Object.defineProperty(prototype, name, { get: () => 999, configurable: true });
      }
    }
    expect(p.getBoundingClientRect().width).toBe(999);
    expect(dom.box(p)).toEqual(before);
    expect(before.width).not.toBe(999);
  });

  /** A matrix interface of its own, like Chromium's SVGMatrix, whose getters work on its objects only. */
  function withScreenMatrix(
    win: DomWindow,
    values: { a: number; b: number; c: number; d: number; e: number; f: number },
  ) {
    const brand = new WeakSet<object>();
    class FakeSVGMatrix {
      constructor() {
        brand.add(this);
      }
    }
    for (const name of ["a", "b", "c", "d", "e", "f"] as const) {
      Object.defineProperty(FakeSVGMatrix.prototype, name, {
        get(this: object) {
          if (!brand.has(this)) throw new TypeError("Illegal invocation");
          return values[name];
        },
        configurable: true,
      });
    }
    const graphics = (win as unknown as { SVGGraphicsElement: { prototype: object } }).SVGGraphicsElement.prototype;
    Object.defineProperty(graphics, "getScreenCTM", { value: () => new FakeSVGMatrix(), configurable: true });
    Object.defineProperty(win, "SVGMatrix", { value: FakeSVGMatrix, configurable: true, writable: true });
    return FakeSVGMatrix;
  }

  test("a screen matrix is read with the getters of its own interface, saved at capture", () => {
    const win = openWindow();
    const FakeSVGMatrix = withScreenMatrix(win, { a: -1, b: 0, c: 0, d: 1, e: 10, f: 20 });
    const dom = captureDom(win);
    const text = win.document.createElementNS("http://www.w3.org/2000/svg", "text");
    win.document.body.appendChild(win.document.createElementNS("http://www.w3.org/2000/svg", "svg")).appendChild(text);
    for (const name of ["a", "d", "e", "f"]) {
      Object.defineProperty(FakeSVGMatrix.prototype, name, { get: () => 1, configurable: true });
    }
    expect(dom.screenMatrix(text)).toEqual({ a: -1, b: 0, c: 0, d: 1, e: 10, f: 20 });
    expect(dom.origin(text)).toEqual({ x: 10, y: 20 });
  });

  test("a matrix of no known interface, or with a value that is not a finite number, is unknown", () => {
    const win = openWindow();
    withScreenMatrix(win, { a: Number.NaN, b: 0, c: 0, d: 1, e: 0, f: 0 });
    const text = win.document.createElementNS("http://www.w3.org/2000/svg", "text");
    win.document.body.appendChild(text);
    expect(captureDom(win).screenMatrix(text)).toBeNull();
    const graphics = (win as unknown as { SVGGraphicsElement: { prototype: object } }).SVGGraphicsElement.prototype;
    Object.defineProperty(graphics, "getScreenCTM", {
      value: () => ({ a: 1, b: 0, c: 0, d: 1, e: 0, f: 0 }),
      configurable: true,
    });
    expect(captureDom(win).screenMatrix(text)).toBeNull();
    expect(captureDom(win).origin(text)).toBeNull();
  });

  test("only an SVG graphics element has a screen matrix, by its interface's brand, not its prototype chain", () => {
    const win = openWindow();
    withScreenMatrix(win, { a: 1, b: 0, c: 0, d: 1, e: 10, f: 20 });
    // Chromium's SVGGraphicsElement getters throw on any other element (a marker, a pattern).
    const brand = new WeakSet<object>();
    const graphics = (win as unknown as { SVGGraphicsElement: { prototype: object } }).SVGGraphicsElement.prototype;
    Object.defineProperty(graphics, "transform", {
      get(this: object) {
        if (!brand.has(this)) throw new TypeError("Illegal invocation");
        return null;
      },
      configurable: true,
    });
    const dom = captureDom(win);
    const svg = win.document.body.appendChild(win.document.createElementNS("http://www.w3.org/2000/svg", "svg"));
    const text = svg.appendChild(win.document.createElementNS("http://www.w3.org/2000/svg", "text"));
    const marker = svg.appendChild(win.document.createElementNS("http://www.w3.org/2000/svg", "marker"));
    brand.add(text);
    expect(dom.screenMatrix(text)).toEqual({ a: 1, b: 0, c: 0, d: 1, e: 10, f: 20 });
    expect(dom.screenMatrix(marker)).toBeNull();
    // The page making the marker look like a graphics element changes nothing.
    Object.setPrototypeOf(marker, graphics);
    expect(dom.screenMatrix(marker)).toBeNull();
    expect(dom.origin(marker)).toBeNull();
  });

  test("a screen matrix that throws is unknown, not an error", () => {
    const win = openWindow();
    const graphics = (win as unknown as { SVGGraphicsElement: { prototype: object } }).SVGGraphicsElement.prototype;
    Object.defineProperty(graphics, "getScreenCTM", {
      value: () => {
        throw new TypeError("Illegal invocation");
      },
      configurable: true,
    });
    const dom = captureDom(win);
    const text = win.document.createElementNS("http://www.w3.org/2000/svg", "text");
    win.document.body.appendChild(win.document.createElementNS("http://www.w3.org/2000/svg", "svg")).appendChild(text);
    expect(dom.screenMatrix(text)).toBeNull();
  });

  test("a text node's fragments come from a Range, read with the functions saved at capture; past 64, none", () => {
    const win = openWindow();
    const lists = new WeakMap<object, DOMRect[]>();
    class FakeDOMRectList {
      constructor(rects: DOMRect[]) {
        lists.set(this, rects);
      }
      get length() {
        return (lists.get(this) as DOMRect[]).length;
      }
      item(index: number) {
        return (lists.get(this) as DOMRect[])[index] ?? null;
      }
    }
    Object.defineProperty(win, "DOMRectList", { value: FakeDOMRectList, configurable: true, writable: true });
    let fragments = 2;
    const selected: Node[] = [];
    Object.defineProperty(win.Range.prototype, "selectNodeContents", {
      value(node: Node) {
        selected.push(node);
      },
      configurable: true,
    });
    Object.defineProperty(win.Range.prototype, "getClientRects", {
      value: () => new FakeDOMRectList(Array.from({ length: fragments }, (_, i) => new win.DOMRect(10 * i, 5, 10, 16))),
      configurable: true,
    });
    const dom = captureDom(win);
    const text = win.document.body.appendChild(win.document.createTextNode("690"));
    Object.defineProperty(FakeDOMRectList.prototype, "length", { get: () => 0, configurable: true });
    Object.defineProperty(win.Range.prototype, "getClientRects", { value: () => [], configurable: true });
    expect(dom.textBoxes(text)).toEqual([
      { left: 0, top: 5, width: 10, height: 16 },
      { left: 10, top: 5, width: 10, height: 16 },
    ]);
    expect(selected).toEqual([text]);
    fragments = 64;
    expect(dom.textBoxes(text)).toHaveLength(64);
    // More fragments than the limit are not read at all, so the caller cannot judge a part of them.
    fragments = 65;
    expect(dom.textBoxes(text)).toBeNull();
  });

  test("a DOM without DOMRectList has no fragments to read", () => {
    const win = openWindow();
    const text = win.document.body.appendChild(win.document.createTextNode("690"));
    expect(captureDom(win).textBoxes(text)).toEqual([]);
  });
});

describe("references, visibility and highlights", () => {
  test("namespaced attributes and ids are read with the functions saved at capture", () => {
    const win = openWindow();
    // A prototype of this document's own, so the shared Document prototype stays as it is.
    Object.setPrototypeOf(win.document, Object.create(Object.getPrototypeOf(win.document)) as object);
    const dom = captureDom(win);
    win.document.body.innerHTML = '<svg><use id="u" xlink:href="#t" href="#h"/></svg>';
    const use = win.document.getElementById("u") as Element;
    Object.defineProperty(win.Element.prototype, "getAttributeNS", { value: () => "#forged", configurable: true });
    Object.defineProperty(Object.getPrototypeOf(win.document), "getElementById", {
      value: () => null,
      configurable: true,
    });
    expect(dom.getAttributeNS(use, "http://www.w3.org/1999/xlink", "href")).toBe("#t");
    expect(dom.getAttributeNS(use, null, "href")).toBe("#h");
    expect(dom.getAttributeNS(use, null, "x")).toBeNull();
    expect(dom.getElementById("u")).toBe(use);
    expect(dom.getElementById("missing")).toBeNull();
  });

  test("visibility is checkVisibility as saved at capture; a DOM without it shows everything", () => {
    const win = openWindow();
    Object.defineProperty(win.Element.prototype, "checkVisibility", {
      value(this: Element) {
        return this.id !== "skipped";
      },
      configurable: true,
    });
    const dom = captureDom(win);
    win.document.body.innerHTML = '<p id="shown">1</p><p id="skipped">2</p>';
    Object.defineProperty(win.Element.prototype, "checkVisibility", { value: () => true, configurable: true });
    expect(dom.visible(win.document.getElementById("shown") as Element)).toBe(true);
    expect(dom.visible(win.document.getElementById("skipped") as Element)).toBe(false);
    Object.defineProperty(win.Element.prototype, "checkVisibility", { value: undefined, configurable: true });
    expect(captureDom(win).visible(win.document.getElementById("skipped") as Element)).toBe(true);
  });

  test("custom highlights are counted with the getters saved at capture; none without CSS.highlights", () => {
    const win = openWindow();
    expect(captureDom(win).highlights()).toBe(0);
    const sizes = new WeakMap<object, number>();
    class FakeHighlightRegistry {
      constructor(size: number) {
        sizes.set(this, size);
      }
      get size() {
        const size = sizes.get(this);
        if (size === undefined) throw new TypeError("Illegal invocation");
        return size;
      }
    }
    const registry = new FakeHighlightRegistry(2);
    // The CSS namespace of this window (happy-dom's is a new object on every read, without highlights).
    const css = {};
    Object.defineProperty(css, "highlights", { get: () => registry, configurable: true });
    Object.defineProperty(win, "CSS", { value: css, configurable: true, writable: true });
    Object.defineProperty(win, "HighlightRegistry", {
      value: FakeHighlightRegistry,
      configurable: true,
      writable: true,
    });
    const dom = captureDom(win);
    Object.defineProperty(FakeHighlightRegistry.prototype, "size", { get: () => 0, configurable: true });
    Object.defineProperty(css, "highlights", { get: () => new FakeHighlightRegistry(0), configurable: true });
    expect(dom.highlights()).toBe(2);
  });
});

describe("computed styles", () => {
  test("a run of properties of one element and pseudo-element reads one computed declaration", () => {
    const win = openWindow();
    const original = win.getComputedStyle;
    const calls: [string, string | null][] = [];
    Object.defineProperty(win, "getComputedStyle", {
      value: (element: Element, pseudo?: string | null) => {
        calls.push([element.id, pseudo ?? null]);
        return original.call(win, element, pseudo);
      },
      configurable: true,
      writable: true,
    });
    const dom = captureDom(win);
    win.document.body.innerHTML = '<p id="a" style="display: block">1</p><p id="b">2</p>';
    const a = win.document.getElementById("a") as Element;
    const b = win.document.getElementById("b") as Element;
    expect(dom.style(a, null, "display")).toBe("block");
    dom.style(a, null, "color");
    dom.style(a, "::before", "content");
    dom.style(a, "::before", "quotes");
    dom.style(b, null, "display");
    dom.style(a, null, "display");
    expect(calls).toEqual([
      ["a", null],
      ["a", "::before"],
      ["b", null],
      ["a", null],
    ]);
  });
});
