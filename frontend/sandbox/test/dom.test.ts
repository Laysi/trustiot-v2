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
});
