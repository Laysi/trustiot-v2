// Closes the ways a page could change what its frame shows without the runtime noticing, or put
// content where the display-time comparison cannot read it (frontend/sandbox/README.md, 「顯示時比對
// 的強制」). The runtime installs it in the sandbox document before any page script runs, in both
// modes, so a page runs under the same realm whether it is checked or shown.
//
// - The CSS object model changes styles without a DOM mutation, so a MutationObserver does not see
//   it: every method and setter that edits a style sheet, a rule or a declaration is wrapped to call
//   `styleChanged` after it ran, and the runtime compares the document again before the next paint.
//   Inline `style` edits are attribute mutations as well; the wrapper only adds a second notice.
// - The Web Animations API starts or changes an animation without a DOM mutation or an event
//   (`element.animate()`, `animation.play()`, `effect.setKeyframes()`): those are wrapped the same
//   way. The comparison counts every live animation (running, paused, pending, in its delay) as a
//   finding (compare.ts, `liveAnimations`), so the frame stays hidden while one can still change
//   what it draws, and the next comparison after it ends checks its final state. CSS animations and
//   transitions start from a style change the runtime already compares after (a DOM mutation, a
//   CSS object model edit, or an event in `POKES`).
// - A closed shadow root is invisible to the comparison in the page's realm: `attachShadow` always
//   makes an open one, which the comparison finds and hides (`unsupported_content`). The parsers
//   that build declarative shadow roots (`setHTMLUnsafe`, `Document.parseHTMLUnsafe`) and
//   `document.write`/`open`, which would replace the document under the runtime, throw.
// - A font built from bytes in script (`new FontFace(name, buffer)` added to `document.fonts`) can
//   draw any digit as another: `FontFaceSet` additions throw. The sandbox's CSP already refuses
//   every font file (`font-src 'none'`).
// - Other states that change which rules match, or what a rule says, without a DOM mutation are
//   wrapped too: an HTML or SVG `<style>` or a `<link>` turned off through its `disabled` property
//   (which is not an attribute; every element interface's own `disabled` setter is wrapped), every
//   setter of a CSS object model interface (including `rule.style = "..."`, which the browser
//   forwards to `cssText` without calling the wrapped setter), a registered custom property
//   (`CSS.registerProperty`), a custom element defined later (`:defined`), validity (`:invalid`)
//   set through `setCustomValidity`, a popover or dialog opened or closed by a method, scrolling
//   by script (`scrollTop`, `scrollTo()`, ...; the scroll event comes only at the next frame), and
//   `designMode`. In Chromium the other element setters that leave no mutation record are of
//   form fields and media (content the comparison refuses anyway), URL parts, `nonce`, `async`,
//   `returnValue` and `zoomAndPan`, none of which changes what is drawn.
// - The SVG DOM's value interfaces (`rect.x.baseVal.value = 5`, transform lists) change an
//   element's attribute without a mutation record in Chromium: their setters and editing methods
//   are wrapped (`isSvgValueInterface`).
// - Some interfaces change what the frame shows in ways the comparison cannot follow, so they
//   throw: a constructed style sheet (`new CSSStyleSheet()`, whose `document.adoptedStyleSheets`
//   array changes without calling any setter; only a constructed sheet can be adopted), a view
//   transition (it keeps a picture of the earlier document on screen while the document already
//   holds other values), and `attachInternals()` (an `ElementInternals` sets a role, a label read
//   aloud, custom states and validity, none of them attributes the comparison or the checker can
//   read). The checker runs pages under this seal too, so a page that uses them fails
//   the check (README, 「顯示時比對的強制」). Custom highlights (`CSS.highlights`) are wrapped: the
//   comparison refuses any registered highlight.
// What remains (an interface a browser adds, or one this list missed) is caught by the runtime's
// comparisons at random times: a finding no notice explains closes the page for good
// (runtime.ts, `readOnlyCompare`).
//
// A child frame's realm is no way around this: the sandbox's CSP sandboxes every frame the page
// creates into an origin of its own, so the page cannot reach a fresh copy of these prototypes there
// (checked in Chromium; the browser test `custom-page.e2e.ts` keeps it checked).
//
// Each replacement is non-writable and non-configurable, so the page cannot put the original back;
// the originals stay in this module's closures.
import type { DomWindow } from "./dom.ts";

const apply = Reflect.apply;
const { defineProperty, getOwnPropertyDescriptor, getOwnPropertyNames, getPrototypeOf } = Object;

export interface SealHooks {
  /**
   * The page may look different without a DOM mutation the runtime's observer sees: a style sheet,
   * rule or declaration changed through the CSS object model, an animation was started or changed,
   * or a shadow root was attached.
   */
  styleChanged(): void;
}

export interface SealOptions {
  /**
   * Whether the replacements are locked (non-configurable). Always in a browser; tests that share
   * DOM prototypes between windows turn it off so they can put the originals back.
   */
  lock: boolean;
  /**
   * Whether the constructors in BLOCKED_CONSTRUCTORS throw. Always in a browser; happy-dom builds a
   * `<style>` element's sheet with its window's `CSSStyleSheet`, so tests on it turn this off.
   */
  constructors?: boolean;
}

/** The constructors whose prototypes hold the CSS object model's editing methods and setters. */
const STYLE_METHODS: readonly (readonly [string, readonly string[]])[] = [
  ["CSSStyleSheet", ["insertRule", "deleteRule", "addRule", "removeRule", "replace", "replaceSync"]],
  ["CSSGroupingRule", ["insertRule", "deleteRule"]],
  ["CSSKeyframesRule", ["appendRule", "deleteRule"]],
  ["CSSStyleDeclaration", ["setProperty", "removeProperty"]],
  ["MediaList", ["appendMedium", "deleteMedium"]],
  ["StylePropertyMap", ["set", "append", "delete", "clear"]],
  ["Element", ["animate", "scroll", "scrollTo", "scrollBy", "scrollIntoView", "scrollIntoViewIfNeeded"]],
  ["Animation", ["play", "pause", "reverse", "finish", "cancel", "persist", "commitStyles", "updatePlaybackRate"]],
  ["AnimationEffect", ["updateTiming"]],
  ["KeyframeEffect", ["setKeyframes"]],
  ["HighlightRegistry", ["set", "delete", "clear"]],
  ["Highlight", ["add", "delete", "clear"]],
  ["CustomElementRegistry", ["define", "upgrade", "initialize"]],
  ["HTMLElement", ["showPopover", "hidePopover", "togglePopover"]],
  ["HTMLDialogElement", ["show", "showModal", "close", "requestClose"]],
  ["HTMLButtonElement", ["setCustomValidity"]],
  ["HTMLFieldSetElement", ["setCustomValidity"]],
  ["HTMLInputElement", ["setCustomValidity"]],
  ["HTMLObjectElement", ["setCustomValidity"]],
  ["HTMLOutputElement", ["setCustomValidity"]],
  ["HTMLSelectElement", ["setCustomValidity"]],
  ["HTMLTextAreaElement", ["setCustomValidity"]],
];

/** Static methods to wrap, on a namespace object of the window. */
const STYLE_STATICS: readonly (readonly [string, readonly string[]])[] = [["CSS", ["registerProperty"]]];

const STYLE_SETTERS: readonly (readonly [string, readonly string[]])[] = [
  ["StyleSheet", ["disabled"]],
  ["CSSStyleRule", ["selectorText"]],
  ["CSSPageRule", ["selectorText"]],
  ["CSSKeyframesRule", ["name"]],
  ["CSSKeyframeRule", ["keyText"]],
  ["CSSRule", ["cssText"]],
  ["MediaList", ["mediaText"]],
  ["Document", ["adoptedStyleSheets"]],
  ["ShadowRoot", ["adoptedStyleSheets"]],
  ["HTMLStyleElement", ["disabled"]],
  ["HTMLLinkElement", ["disabled"]],
  ["SVGStyleElement", ["disabled"]],
  ["Element", ["scrollTop", "scrollLeft"]],
  ["Document", ["designMode"]],
  ["SVGSVGElement", ["currentScale"]],
];

/**
 * Interfaces whose every setter changes a style or an animation; with every interface whose name
 * starts with `CSS` (`isStyleInterface`), so a rule type a browser adds is wrapped as well.
 */
const SETTER_INTERFACES = [
  "StyleSheet",
  "MediaList",
  "Animation",
  "AnimationEffect",
  "KeyframeEffect",
  "AnimationTimeline",
  "ScrollTimeline",
  "ViewTimeline",
  "Highlight",
] as const;

const isStyleInterface = (name: string) => /^CSS[A-Z]/.test(name);

/**
 * The SVG DOM's value interfaces (`SVGLength`, `SVGAnimatedNumber`, `SVGTransformList`, ...): writing
 * through them changes an element's attribute without a mutation record (Chromium synchronizes the
 * attribute lazily), so a mark's geometry would change unnoticed. Their every setter and these
 * methods are wrapped; element interfaces (`SVG...Element`) are not swept.
 */
const isSvgValueInterface = (name: string) => /^SVG[A-Z]/.test(name) && !name.endsWith("Element");

const SVG_VALUE_METHODS = [
  "appendItem",
  "insertItemBefore",
  "replaceItem",
  "removeItem",
  "clear",
  "initialize",
  "consolidate",
  "newValueSpecifiedUnits",
  "convertToSpecifiedUnits",
  "setMatrix",
  "setTranslate",
  "setScale",
  "setRotate",
  "setSkewX",
  "setSkewY",
  "setOrientToAuto",
  "setOrientToAngle",
] as const;

/** The prototypes whose every setter is a style property (`style.color = …`, `style.cssText = …`). */
const DECLARATIONS = ["CSSStyleDeclaration", "CSSStyleProperties", "CSS2Properties"] as const;

/** Methods that would build content the comparison cannot read, or replace the document. */
const BLOCKED_METHODS: readonly (readonly [string, readonly string[]])[] = [
  ["Element", ["setHTMLUnsafe"]],
  ["ShadowRoot", ["setHTMLUnsafe"]],
  ["Document", ["write", "writeln", "open"]],
  ["HTMLDocument", ["write", "writeln", "open"]],
  ["FontFaceSet", ["add"]],
  ["Document", ["startViewTransition"]],
  ["Element", ["startViewTransition"]],
  ["HTMLElement", ["attachInternals"]],
];

/** Constructors that throw: `new CSSStyleSheet()` (see the module documentation). */
const BLOCKED_CONSTRUCTORS = ["CSSStyleSheet"] as const;

/** Static methods to block, on the constructor itself. */
const BLOCKED_STATICS: readonly (readonly [string, readonly string[]])[] = [["Document", ["parseHTMLUnsafe"]]];

/** The shadow root options `attachShadow` passes on; `mode` is always `open`. */
const SHADOW_OPTIONS = ["delegatesFocus", "slotAssignment", "clonable", "serializable"] as const;

function prototypeOf(win: DomWindow, name: string): object | null {
  const ctor = (win as unknown as Record<string, unknown>)[name];
  if (typeof ctor !== "function") return null;
  const prototype = (ctor as { prototype?: unknown }).prototype;
  return typeof prototype === "object" && prototype !== null ? prototype : null;
}

/**
 * The object on `start`'s prototype chain that owns `name`: in a browser the interface prototype
 * itself; a test DOM may define it on a base class.
 */
function ownerOf(start: object, name: string): object | null {
  for (let current: object | null = start; current !== null && current !== Object.prototype; ) {
    if (getOwnPropertyDescriptor(current, name) !== undefined) return current;
    current = getPrototypeOf(current) as object | null;
  }
  return null;
}

/** Installs the seal in `win`; see the module documentation. */
export function seal(win: DomWindow, hooks: SealHooks, options: SealOptions = { lock: true }): void {
  const promiseThen = (win as unknown as { Promise: PromiseConstructor }).Promise.prototype.then;
  const notify = () => hooks.styleChanged();
  // A member is replaced once, whichever entry reaches it first: a locked one cannot be redefined.
  const replaced = new WeakMap<object, Set<string>>();
  const replace = (target: object, name: string, descriptor: PropertyDescriptor) => {
    const original = getOwnPropertyDescriptor(target, name);
    if (original === undefined) return;
    const names = replaced.get(target) ?? new Set<string>();
    if (names.has(name)) return;
    names.add(name);
    replaced.set(target, names);
    const locked: PropertyDescriptor =
      "value" in descriptor
        ? { value: descriptor.value, writable: !options.lock, enumerable: original.enumerable }
        : { get: descriptor.get, set: descriptor.set, enumerable: original.enumerable };
    defineProperty(target, name, { ...locked, configurable: !options.lock });
  };

  const wrapMethod = (start: object, name: string) => {
    const target = ownerOf(start, name);
    if (target === null) return;
    const original = getOwnPropertyDescriptor(target, name)?.value as unknown;
    if (typeof original !== "function") return;
    const wrapped = {
      [name](this: unknown, ...args: unknown[]) {
        const result = apply(original as (...a: unknown[]) => unknown, this, args);
        notify();
        // replace() applies the sheet when its promise settles.
        if (typeof result === "object" && result !== null && name === "replace") {
          try {
            apply(promiseThen, result, [notify, notify]);
          } catch {
            // Not a promise of this realm: the notice above stands.
          }
        }
        return result;
      },
    }[name];
    replace(target, name, { value: wrapped });
  };

  const wrapSetter = (start: object, name: string) => {
    const target = ownerOf(start, name);
    if (target === null) return;
    const original = getOwnPropertyDescriptor(target, name);
    const set = original?.set;
    if (original === undefined || set === undefined) return;
    replace(target, name, {
      get: original.get,
      set(this: unknown, value: unknown) {
        apply(set, this, [value]);
        notify();
      },
    });
  };

  for (const [ctor, names] of STYLE_METHODS) {
    const prototype = prototypeOf(win, ctor);
    if (prototype !== null) for (const name of names) wrapMethod(prototype, name);
  }
  for (const [ctor, names] of STYLE_SETTERS) {
    const prototype = prototypeOf(win, ctor);
    if (prototype !== null) for (const name of names) wrapSetter(prototype, name);
  }
  const interfaces = new Set<string>([...DECLARATIONS, ...SETTER_INTERFACES]);
  let globals: string[] = [];
  try {
    globals = getOwnPropertyNames(win);
  } catch {
    // A test window without own names: the fixed lists only.
  }
  for (const name of globals) if (isStyleInterface(name) || isSvgValueInterface(name)) interfaces.add(name);
  // A `disabled` that is not an attribute turns a style sheet off (HTMLStyleElement, SVGStyleElement);
  // every element interface's own `disabled` setter is wrapped, so one a browser adds is too. The
  // ones STYLE_SETTERS already wrapped are skipped by `replace`, which replaces a member once.
  for (const name of globals) {
    if (!name.endsWith("Element")) continue;
    const prototype = prototypeOf(win, name);
    if (prototype !== null && getOwnPropertyDescriptor(prototype, "disabled")?.set !== undefined) {
      wrapSetter(prototype, "disabled");
    }
  }
  for (const name of globals) {
    if (!isSvgValueInterface(name)) continue;
    const prototype = prototypeOf(win, name);
    if (prototype === null) continue;
    for (const method of SVG_VALUE_METHODS) {
      if (typeof getOwnPropertyDescriptor(prototype, method)?.value === "function") wrapMethod(prototype, method);
    }
  }
  for (const ctor of interfaces) {
    const prototype = prototypeOf(win, ctor);
    if (prototype === null) continue;
    for (const name of getOwnPropertyNames(prototype)) {
      if (getOwnPropertyDescriptor(prototype, name)?.set !== undefined) wrapSetter(prototype, name);
    }
  }
  for (const [namespace, names] of STYLE_STATICS) {
    const target = (win as unknown as Record<string, unknown>)[namespace];
    if (typeof target !== "object" || target === null) continue;
    for (const name of names) {
      const original = getOwnPropertyDescriptor(target, name)?.value as unknown;
      if (typeof original !== "function") continue;
      replace(target, name, {
        value: {
          [name](this: unknown, ...args: unknown[]) {
            const result = apply(original as (...a: unknown[]) => unknown, this, args);
            notify();
            return result;
          },
        }[name],
      });
    }
  }

  const refuse = (what: string) =>
    function refused(): never {
      throw new TypeError(`${what} is not available in the dashboard sandbox`);
    };
  for (const [ctor, names] of BLOCKED_METHODS) {
    const prototype = prototypeOf(win, ctor);
    if (prototype === null) continue;
    for (const name of names) {
      const owner = ownerOf(prototype, name);
      if (owner !== null) replace(owner, name, { value: refuse(name) });
    }
  }
  for (const [ctor, names] of BLOCKED_STATICS) {
    const target = (win as unknown as Record<string, unknown>)[ctor];
    if (typeof target !== "function") continue;
    for (const name of names) {
      if (getOwnPropertyDescriptor(target, name) !== undefined) replace(target, name, { value: refuse(name) });
    }
  }

  for (const name of options.constructors === false ? [] : BLOCKED_CONSTRUCTORS) {
    const original = (win as unknown as Record<string, unknown>)[name];
    if (typeof original !== "function") continue;
    const prototype = (original as { prototype?: unknown }).prototype;
    // `instanceof` keeps working: the replacement has the same prototype.
    const blocked = function constructorRefused(): never {
      throw new TypeError(`new ${name}() is not available in the dashboard sandbox`);
    };
    defineProperty(blocked, "prototype", { value: prototype });
    replace(win, name, { value: blocked });
    // `sheet.constructor` reaches the original otherwise.
    if (typeof prototype === "object" && prototype !== null) replace(prototype, "constructor", { value: blocked });
  }

  // attachShadow: always an open root, which the comparison reads (and hides as unsupported).
  const elementPrototype = prototypeOf(win, "Element");
  const element = elementPrototype === null ? null : ownerOf(elementPrototype, "attachShadow");
  const attachShadow =
    element === null ? undefined : (getOwnPropertyDescriptor(element, "attachShadow")?.value as unknown);
  if (element !== null && typeof attachShadow === "function") {
    replace(element, "attachShadow", {
      value: function attachShadowOpen(this: unknown, init: unknown) {
        const options: Record<string, unknown> = { mode: "open" };
        if (typeof init === "object" && init !== null) {
          for (const key of SHADOW_OPTIONS) {
            const value = (init as Record<string, unknown>)[key];
            if (value !== undefined) options[key] = value;
          }
        }
        const root = apply(attachShadow as (o: unknown) => unknown, this, [options]);
        // A shadow root changes what the host element draws without a mutation in the document's
        // tree: compare again (the comparison hides an element with one).
        notify();
        return root;
      },
    });
  }
}
