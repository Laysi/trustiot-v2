// The seal (src/seal.ts; README 「顯示時比對的強制」): style edits through the CSS object model are
// reported, a shadow root is always open, the parsers that build closed ones and `document.write`
// throw, and in a browser nothing of it can be undone. happy-dom shares DOM prototypes between
// windows, so the happy-dom tests seal without the lock and put every prototype back after
// (`guardPrototypes`); the lock is checked on a stand-in window with prototypes of its own.
import { afterEach, describe, expect, test } from "bun:test";

import { Window } from "happy-dom";

import type { DomWindow } from "../src/dom.ts";
import { seal } from "../src/seal.ts";
import { guardPrototypes } from "./harness.ts";

let cleanup: (() => Promise<void>) | null = null;

afterEach(async () => {
  await cleanup?.();
  cleanup = null;
});

function sealedWindow() {
  const happy = new Window({ url: "https://sandbox.example.test/sandbox/v1/sandbox.html" });
  const restore = guardPrototypes(happy);
  let changes = 0;
  // happy-dom builds a <style>'s sheet with the window's CSSStyleSheet: the constructor stays.
  seal(happy as unknown as DomWindow, { styleChanged: () => changes++ }, { lock: false, constructors: false });
  cleanup = async () => {
    restore();
    await happy.happyDOM.abort();
    await happy.happyDOM.close();
  };
  return { win: happy as unknown as typeof globalThis, changes: () => changes };
}

describe("style edits", () => {
  test("a rule inserted or deleted through a style sheet is reported", () => {
    const { win, changes } = sealedWindow();
    const style = win.document.createElement("style");
    win.document.head.appendChild(style);
    const sheet = style.sheet as CSSStyleSheet;
    const before = changes();
    sheet.insertRule("p { color: red }", 0);
    expect(changes()).toBe(before + 1);
    sheet.deleteRule(0);
    expect(changes()).toBe(before + 2);
  });

  test("a declaration changed through a property setter, setProperty or cssText is reported", () => {
    const { win, changes } = sealedWindow();
    const p = win.document.createElement("p");
    const before = changes();
    p.style.color = "red";
    p.style.setProperty("font-size", "20px");
    p.style.cssText = "color: blue";
    expect(changes()).toBe(before + 3);
    // The edits themselves still work.
    expect(p.style.color).toBe("blue");
  });

  test("an animation started through the Web Animations API is reported", () => {
    const { win, changes } = sealedWindow();
    const p = win.document.createElement("p");
    win.document.body.appendChild(p);
    const before = changes();
    p.animate([{ opacity: 0 }, { opacity: 1 }], 1000);
    // At least once: animate() may itself call play(), which reports as well.
    expect(changes()).toBeGreaterThan(before);
  });
});

describe("content the comparison could not read", () => {
  test("attachShadow always makes an open root, and reports it", () => {
    const sealed = sealedWindow();
    const { win } = sealed;
    const host = win.document.createElement("div");
    const { changes } = sealed;
    const before = changes();
    const root = host.attachShadow({ mode: "closed" });
    expect(host.shadowRoot).toBe(root);
    // And the runtime compares again.
    expect(changes()).toBe(before + 1);
  });

  test("document.write, writeln and open throw", () => {
    const { win } = sealedWindow();
    expect(() => win.document.write("<p>1</p>")).toThrow(TypeError);
    expect(() => win.document.writeln("<p>1</p>")).toThrow(TypeError);
    expect(() => win.document.open()).toThrow(TypeError);
  });
});

describe("the lock", () => {
  /** A window whose DOM classes are its own, as each browser realm's are. */
  function standIn() {
    class Element {
      attachShadow(init: { mode: string }) {
        return { mode: init.mode };
      }
      setHTMLUnsafe(_html: string) {}
    }
    class CSSStyleSheet {
      insertRule() {
        return 0;
      }
    }
    class CSSStyleDeclaration {
      #color = "";
      get color() {
        return this.#color;
      }
      set color(value: string) {
        this.#color = value;
      }
    }
    class Document {
      write() {}
      static parseHTMLUnsafe() {
        return null;
      }
    }
    class FontFaceSet {
      add() {}
    }
    class Animation {
      play() {}
    }
    // As in a browser, HTMLDocument inherits write from Document: sealing it twice must not throw.
    class HTMLDocument extends Document {}
    const win = {
      Element,
      CSSStyleSheet,
      CSSStyleDeclaration,
      Document,
      HTMLDocument,
      FontFaceSet,
      Animation,
      Promise,
    };
    return { win, Element, CSSStyleSheet, CSSStyleDeclaration, Document, FontFaceSet, Animation };
  }

  test("every replacement is non-configurable, and a page cannot put the original back", () => {
    const { win, Element, CSSStyleSheet, CSSStyleDeclaration, Document, FontFaceSet, Animation } = standIn();
    let changes = 0;
    seal(win as unknown as DomWindow, { styleChanged: () => changes++ });
    for (const [target, name] of [
      [Element.prototype, "attachShadow"],
      [Element.prototype, "setHTMLUnsafe"],
      [CSSStyleSheet.prototype, "insertRule"],
      [CSSStyleDeclaration.prototype, "color"],
      [Document.prototype, "write"],
      [Document, "parseHTMLUnsafe"],
      [FontFaceSet.prototype, "add"],
      [Animation.prototype, "play"],
    ] as const) {
      const descriptor = Object.getOwnPropertyDescriptor(target, name);
      expect(descriptor?.configurable, name).toBe(false);
      if (descriptor !== undefined && "value" in descriptor) expect(descriptor.writable, name).toBe(false);
      expect(() => Object.defineProperty(target, name, { value: () => "original" }), name).toThrow(TypeError);
    }
    expect(new Element().attachShadow({ mode: "closed" })).toEqual({ mode: "open" });
    expect(() => new Element().setHTMLUnsafe("<template shadowrootmode=closed></template>")).toThrow(TypeError);
    expect(() => Document.parseHTMLUnsafe()).toThrow(TypeError);
    expect(() => new FontFaceSet().add()).toThrow(TypeError);
    const declaration = new CSSStyleDeclaration();
    declaration.color = "red";
    expect(declaration.color).toBe("red");
    new CSSStyleSheet().insertRule();
    expect(changes).toBe(3);
    // An animation started through the Web Animations API is reported too.
    new Animation().play();
    expect(changes).toBe(4);
  });
});

describe("changes without a DOM mutation, and interfaces the comparison cannot follow", () => {
  /** Stand-ins with the browser's shapes for the interfaces the seal wraps or refuses (H1 of the #997 review). */
  function standIn() {
    class HTMLStyleElement {
      #disabled = false;
      get disabled() {
        return this.#disabled;
      }
      set disabled(value: boolean) {
        this.#disabled = value;
      }
    }
    class CSSStyleRule {
      #text = "";
      get style(): unknown {
        return { cssText: this.#text };
      }
      // A browser forwards `rule.style = text` to cssText without calling any setter it exposes.
      set style(text: unknown) {
        this.#text = String(text);
      }
    }
    class CSSStyleSheet {
      replaceSync() {}
    }
    class CustomElementRegistry {
      define() {}
    }
    // Not an attribute in a browser either; found by the sweep of element interfaces' `disabled`.
    class SVGStyleElement {
      #disabled = false;
      get disabled() {
        return this.#disabled;
      }
      set disabled(value: boolean) {
        this.#disabled = value;
      }
    }
    class Element {
      #top = 0;
      get scrollTop() {
        return this.#top;
      }
      set scrollTop(value: number) {
        this.#top = value;
      }
      scrollBy() {}
    }
    class HTMLElement extends Element {
      showPopover() {}
      attachInternals() {
        return {};
      }
    }
    class Document {
      startViewTransition() {
        return "transition";
      }
      #design = "off";
      get designMode() {
        return this.#design;
      }
      set designMode(value: string) {
        this.#design = value;
      }
    }
    class HighlightRegistry {
      set() {}
    }
    const css = { registerProperty: () => undefined };
    const win = {
      HTMLStyleElement,
      CSSStyleRule,
      CSSStyleSheet,
      CustomElementRegistry,
      SVGStyleElement,
      Element,
      HTMLElement,
      Document,
      HighlightRegistry,
      CSS: css,
      Promise,
    };
    return {
      win,
      HTMLStyleElement,
      CSSStyleRule,
      CSSStyleSheet,
      CustomElementRegistry,
      SVGStyleElement,
      Element,
      HTMLElement,
      Document,
      HighlightRegistry,
      css,
    };
  }

  test("each is reported, and still does what it did", () => {
    const parts = standIn();
    let changes = 0;
    seal(parts.win as unknown as DomWindow, { styleChanged: () => changes++ });
    const style = new parts.HTMLStyleElement();
    style.disabled = true;
    expect(style.disabled).toBe(true);
    // Once: STYLE_SETTERS and the sweep of `disabled` setters both reach it, and it is wrapped once.
    expect(changes).toBe(1);
    const rule = new parts.CSSStyleRule();
    rule.style = "content: '9'";
    expect((rule.style as { cssText: string }).cssText).toBe("content: '9'");
    expect(changes).toBe(2);
    parts.css.registerProperty();
    new parts.CustomElementRegistry().define();
    new parts.HTMLElement().showPopover();
    new parts.HighlightRegistry().set();
    const doc = new parts.Document();
    doc.designMode = "on";
    expect(changes).toBe(7);
    const svgStyle = new parts.SVGStyleElement();
    svgStyle.disabled = true;
    expect(svgStyle.disabled).toBe(true);
    expect(changes).toBe(8);
    const box = new parts.HTMLElement();
    box.scrollTop = 5;
    expect(box.scrollTop).toBe(5);
    box.scrollBy();
    expect(changes).toBe(10);
  });

  test("a constructed style sheet, a view transition and element internals throw, and instanceof still holds", () => {
    const parts = standIn();
    const sheetPrototype = parts.CSSStyleSheet.prototype;
    seal(parts.win as unknown as DomWindow, { styleChanged: () => {} });
    const Constructor = parts.win.CSSStyleSheet as unknown as new () => object;
    expect(() => new Constructor()).toThrow(TypeError);
    // Through a sheet's own `constructor` too.
    const Again = sheetPrototype.constructor as unknown as new () => object;
    expect(() => new Again()).toThrow(TypeError);
    expect(Object.create(sheetPrototype) instanceof Constructor).toBe(true);
    expect(() => Object.defineProperty(parts.win, "CSSStyleSheet", { value: parts.CSSStyleSheet })).toThrow(TypeError);
    expect(() => new parts.Document().startViewTransition()).toThrow(TypeError);
    expect(() => new parts.HTMLElement().attachInternals()).toThrow(TypeError);
  });
});

describe("no motion through the Web Animations API (README, 「顯示時比對的強制」)", () => {
  /** Stand-ins with the browser's shapes: an effect's timing, and a rate change that waits for a frame. */
  function standIn() {
    type Timing = Record<string, unknown>;
    const MEMBERS = ["delay", "direction", "duration", "easing", "endDelay", "fill", "iterationStart", "iterations"];
    // What the browser does inside, not through the methods a page sees (and the seal wraps).
    const setTiming = (timing: Timing, update: Timing | number | undefined) => {
      const members: Timing = typeof update === "number" ? { duration: update } : (update ?? {});
      for (const key of MEMBERS) {
        const value = members[key];
        if (value === undefined) continue;
        if (key === "duration" && typeof value === "number" && value < 0) throw new TypeError("negative duration");
        timing[key] = value;
      }
    };
    class AnimationEffect {
      readonly timing: Timing = { delay: 0, duration: "auto", endDelay: 0, fill: "auto", iterations: 1 };
      updateTiming(update?: Timing) {
        setTiming(this.timing, update);
      }
    }
    class KeyframeEffect extends AnimationEffect {
      constructor(
        readonly target: unknown,
        readonly keyframes: unknown,
        options?: Timing | number,
      ) {
        super();
        setTiming(this.timing, options);
      }
    }
    class Animation {
      #rate = 1;
      #pending: number | null = null;
      constructor(readonly made: AnimationEffect | null = null) {}
      get effect() {
        return this.made;
      }
      get playbackRate() {
        return this.#rate;
      }
      set playbackRate(value: number) {
        this.#pending = null;
        this.#rate = value;
      }
      updatePlaybackRate(value: number) {
        if (typeof value !== "number" || !Number.isFinite(value)) throw new TypeError("not a finite double");
        this.#pending = value;
      }
      reverse() {
        this.#pending = -(this.#pending ?? this.#rate);
      }
      play() {}
      /** The browser's next frame: a pending rate is applied. */
      nextFrame() {
        if (this.#pending !== null) this.#rate = this.#pending;
        this.#pending = null;
      }
    }
    class Element {
      animate(keyframes: unknown, options?: Timing | number) {
        return new Animation(new KeyframeEffect(this, keyframes, options));
      }
    }
    const win = { Element, Animation, AnimationEffect, KeyframeEffect, Promise };
    let changes = 0;
    seal(win as unknown as DomWindow, { styleChanged: () => changes++ });
    return { win, Element, Animation, AnimationEffect, KeyframeEffect, changes: () => changes };
  }
  const NONE = { duration: 0, delay: 0, endDelay: 0 };

  test("animate() gives the effect no duration, delay or end delay, as a number or options, and keeps the rest", () => {
    const { Element, changes } = standIn();
    const element = new Element();
    expect(element.animate([], 3000).effect?.timing).toMatchObject(NONE);
    expect(element.animate([]).effect?.timing).toMatchObject(NONE);
    const before = changes();
    const options = {
      duration: 3000,
      delay: 500,
      endDelay: 200,
      fill: "forwards",
      iterations: 3,
      easing: "ease-in",
      direction: "alternate",
    };
    expect(element.animate([], options).effect?.timing).toEqual({ ...options, ...NONE });
    expect(changes()).toBe(before + 1);
    // The page's options are left as they were, and the browser's own checks still throw.
    expect(options.duration).toBe(3000);
    expect(() => element.animate([], { duration: -1 })).toThrow(TypeError);
  });

  test("updateTiming() keeps every member but those three, and cannot bring them back", () => {
    const { Element, changes } = standIn();
    const effect = new Element().animate([], { fill: "both" }).effect;
    const before = changes();
    effect?.updateTiming({ duration: 1000, delay: 10, endDelay: 5, iterations: 2, easing: "steps(2)" });
    expect(effect?.timing).toEqual({ ...NONE, fill: "both", iterations: 2, easing: "steps(2)" });
    expect(changes()).toBe(before + 1);
  });

  test("new KeyframeEffect() gives no motion, and instanceof, constructor and subclasses still hold", () => {
    const { win, KeyframeEffect } = standIn();
    const Sealed = win.KeyframeEffect as unknown as typeof KeyframeEffect;
    expect(Sealed).not.toBe(KeyframeEffect);
    const effect = new Sealed({}, [], { duration: 800, delay: 100, fill: "both" });
    expect(effect.timing).toMatchObject({ ...NONE, fill: "both" });
    expect(effect instanceof Sealed).toBe(true);
    expect(effect instanceof KeyframeEffect).toBe(true);
    expect(Object.getPrototypeOf(effect)).toBe(KeyframeEffect.prototype);
    expect(effect.constructor).toBe(Sealed);
    // Through an effect's own `constructor` too.
    const Again = effect.constructor as typeof KeyframeEffect;
    expect(new Again({}, [], 500).timing).toMatchObject(NONE);
    class Sub extends Sealed {}
    const sub = new Sub({}, [], { duration: 900 });
    expect(sub instanceof Sub).toBe(true);
    expect(sub.timing).toMatchObject(NONE);
    // Without `new` it throws, as the browser's constructor does.
    expect(() => (Sealed as unknown as (...args: unknown[]) => unknown)({}, [])).toThrow(TypeError);
    expect(() => Object.defineProperty(Sealed, "prototype", { value: {} })).toThrow(TypeError);
  });

  test("a playback rate change applies at once, so the rate read is the one it plays at", () => {
    const { Element, changes } = standIn();
    const animation = new Element().animate([], 1000);
    const before = changes();
    animation.updatePlaybackRate(-1);
    expect(animation.playbackRate).toBe(-1);
    animation.nextFrame();
    expect(animation.playbackRate).toBe(-1);
    animation.reverse();
    expect(animation.playbackRate).toBe(1);
    animation.nextFrame();
    expect(animation.playbackRate).toBe(1);
    expect(changes()).toBe(before + 2);
    // The argument is converted once; the browser's own check still throws.
    let conversions = 0;
    animation.updatePlaybackRate({
      valueOf: () => {
        conversions++;
        return 2;
      },
    } as unknown as number);
    expect(conversions).toBe(1);
    expect(animation.playbackRate).toBe(2);
    expect(() => animation.updatePlaybackRate(Number.NaN)).toThrow(TypeError);
    expect(animation.playbackRate).toBe(2);
  });

  test("every replacement is locked", () => {
    const { win, Element, Animation, AnimationEffect, KeyframeEffect } = standIn();
    for (const [target, name] of [
      [Element.prototype, "animate"],
      [AnimationEffect.prototype, "updateTiming"],
      [Animation.prototype, "updatePlaybackRate"],
      [Animation.prototype, "reverse"],
      [KeyframeEffect.prototype, "constructor"],
      [win, "KeyframeEffect"],
    ] as const) {
      const descriptor = Object.getOwnPropertyDescriptor(target, name);
      expect(descriptor?.configurable, name).toBe(false);
      expect(descriptor?.writable, name).toBe(false);
      expect(() => Object.defineProperty(target, name, { value: () => "original" }), name).toThrow(TypeError);
    }
  });
});
