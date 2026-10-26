// Test support: the sandbox runtime in a fresh happy-dom window, and the parts of the checking
// service's flow (frontend/sandbox/README.md, 「檢查服務的流程」) that a DOM without a browser can
// show. Each window has its own JavaScript realm, but happy-dom shares the prototypes of its base
// DOM classes (Node, Element, ...) between windows, so a frame puts back every DOM prototype
// property a page changed when it closes (`guardPrototypes`). happy-dom has no layout: `layoutDom` reads SVG geometry from
// the attributes instead, which is enough for the rules on marks and axes; the real layout is
// covered by the checking service's image test with the same fixtures.
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { Window } from "happy-dom";

import { compareDocument, SVG_NS, verdictOf } from "../src/compare.ts";
import { type Box, captureDom, type DomAccess, type DomWindow, type Point } from "../src/dom.ts";
import {
  type CheckRequest,
  type CheckVerdict,
  type DatasetResult,
  PROTOCOL,
  type Problem,
  QUIET_PERIOD_MS,
  RENDER_TIME_LIMIT_MS,
  type SandboxMessage,
} from "../src/protocol.ts";
import { type Runtime, startRuntime } from "../src/runtime.ts";

const ENTRIES = fileURLToPath(new URL("../src/entries/", import.meta.url));

let d3Source: Promise<string> | undefined;

/** The d3 bundle the sandbox document loads (entries/d3.ts), built once per test process. */
export function d3Bundle(): Promise<string> {
  d3Source ??= (async () => {
    const result = await Bun.build({ entrypoints: [join(ENTRIES, "d3.ts")], format: "iife", target: "browser" });
    const output = result.outputs[0];
    if (!result.success || output === undefined) throw new AggregateError(result.logs, "bundling d3 failed");
    return output.text();
  })();
  return d3Source;
}

const TRANSLATE = /^\s*translate\(\s*(-?[\d.]+(?:e[-+]?\d+)?)(?:[\s,]+(-?[\d.]+(?:e[-+]?\d+)?))?\s*\)\s*$/i;

/**
 * `base` with geometry from SVG attributes: the boxes of rect, circle, ellipse, line and text,
 * and the origins of groups, moved by every `translate(...)` above them. HTML elements and other
 * shapes have an empty box, as happy-dom gives every element.
 */
export function layoutDom(base: DomAccess): DomAccess {
  const number = (element: Element, name: string) => {
    const value = Number(base.getAttribute(element, name) ?? "0");
    return Number.isFinite(value) ? value : 0;
  };
  const offset = (element: Element, includeSelf: boolean): Point => {
    let x = 0;
    let y = 0;
    for (let node: Node | null = includeSelf ? element : base.parentNode(element); node !== null; ) {
      if (base.nodeType(node) !== 1) break;
      const match = TRANSLATE.exec(base.getAttribute(node as Element, "transform") ?? "");
      if (match !== null) {
        x += Number(match[1]);
        y += Number(match[2] ?? "0");
      }
      node = base.parentNode(node);
    }
    return { x, y };
  };
  return {
    ...base,
    box(element: Element): Box {
      if (base.namespaceURI(element) !== SVG_NS) return { left: 0, top: 0, width: 0, height: 0 };
      const at = offset(element, true);
      const name = base.localName(element);
      const place = (left: number, top: number, width: number, height: number): Box => ({
        left: left + at.x,
        top: top + at.y,
        width,
        height,
      });
      switch (name) {
        case "rect":
          return place(number(element, "x"), number(element, "y"), number(element, "width"), number(element, "height"));
        case "circle": {
          const r = number(element, "r");
          return place(number(element, "cx") - r, number(element, "cy") - r, 2 * r, 2 * r);
        }
        case "ellipse": {
          const rx = number(element, "rx");
          const ry = number(element, "ry");
          return place(number(element, "cx") - rx, number(element, "cy") - ry, 2 * rx, 2 * ry);
        }
        case "line": {
          const [x1, x2, y1, y2] = ["x1", "x2", "y1", "y2"].map((attribute) => number(element, attribute)) as [
            number,
            number,
            number,
            number,
          ];
          return place(Math.min(x1, x2), Math.min(y1, y2), Math.abs(x2 - x1), Math.abs(y2 - y1));
        }
        case "text":
          return place(number(element, "x"), number(element, "y"), 0, 0);
        default:
          return { left: 0, top: 0, width: 0, height: 0 };
      }
    },
    origin(element: Element): Point | null {
      return base.namespaceURI(element) === SVG_NS ? offset(element, true) : null;
    },
  };
}

export interface Frame {
  window: DomWindow;
  /** Captured before the runtime and the page ran, like the isolated world's. */
  dom: DomAccess;
  runtime: Runtime;
  /** Everything the runtime posted, in order. */
  messages: SandboxMessage[];
  /**
   * The first message from index `from` on that `accept` takes, or `null` once `limitMs` passed:
   * the limit is the rule's own (for example the render time limit), not a guess at speed.
   */
  next(accept: (message: SandboxMessage) => boolean, limitMs: number, from?: number): Promise<SandboxMessage | null>;
  close(): Promise<void>;
}

/**
 * Records every own property of the prototypes of `win`'s DOM classes (and of their own
 * prototype chains); the returned function puts them back as they were and deletes any added.
 */
export function guardPrototypes(win: object): () => void {
  const saved = new Map<object, Map<PropertyKey, PropertyDescriptor>>();
  const record = (target: object | null) => {
    for (let current = target; current !== null && !saved.has(current); current = Object.getPrototypeOf(current)) {
      if (current === Object.prototype || current === Function.prototype) break;
      const descriptors = new Map<PropertyKey, PropertyDescriptor>();
      for (const key of Reflect.ownKeys(current)) {
        const descriptor = Reflect.getOwnPropertyDescriptor(current, key);
        if (descriptor !== undefined) descriptors.set(key, descriptor);
      }
      saved.set(current, descriptors);
    }
  };
  for (const name of Object.getOwnPropertyNames(win)) {
    if (!/^[A-Z]/.test(name)) continue;
    const value = Reflect.getOwnPropertyDescriptor(win, name)?.value as unknown;
    if (typeof value === "function" && typeof value.prototype === "object" && value.prototype !== null) {
      record(value.prototype);
      record(value);
    }
  }
  return () => {
    for (const [target, descriptors] of saved) {
      for (const key of Reflect.ownKeys(target)) {
        if (!descriptors.has(key)) Reflect.deleteProperty(target, key);
      }
      for (const [key, descriptor] of descriptors) {
        const now = Reflect.getOwnPropertyDescriptor(target, key);
        const same =
          now !== undefined &&
          now.value === descriptor.value &&
          now.get === descriptor.get &&
          now.set === descriptor.set;
        if (!same) Reflect.defineProperty(target, key, descriptor);
      }
    }
  };
}

/**
 * A sandbox document (`#tq-root` in an empty body) with the runtime started in it; `beforeStart`
 * runs on the window first, as a browser's own code would be in place before the runtime.
 */
export async function openFrame(beforeStart: (win: DomWindow) => void = () => {}): Promise<Frame> {
  const happy = new Window({ url: "https://sandbox.example.test/sandbox/v1/sandbox.html", width: 1200, height: 800 });
  const restorePrototypes = guardPrototypes(happy);
  const win = happy as unknown as DomWindow;
  const root = win.document.createElement("div");
  root.id = "tq-root";
  win.document.body.appendChild(root);
  const dom = layoutDom(captureDom(win));
  happy.eval(await d3Bundle());
  beforeStart(win);
  const messages: SandboxMessage[] = [];
  const waiting = new Set<() => void>();
  const runtime = startRuntime({
    window: win,
    post: (message) => {
      messages.push(message);
      for (const wake of [...waiting]) wake();
    },
    d3: (win as unknown as { d3: unknown }).d3,
    // happy-dom has no layout.
    layout: false,
  });
  const next = (accept: (message: SandboxMessage) => boolean, limitMs: number, from = 0) =>
    new Promise<SandboxMessage | null>((resolve) => {
      let index = from;
      const look = () => {
        for (; index < messages.length; index++) {
          const message = messages[index];
          if (message !== undefined && accept(message)) {
            finish(message);
            return;
          }
        }
      };
      const timer = setTimeout(() => finish(null), limitMs);
      const finish = (message: SandboxMessage | null) => {
        clearTimeout(timer);
        waiting.delete(look);
        resolve(message);
      };
      waiting.add(look);
      look();
    });
  return {
    window: win,
    dom,
    runtime,
    messages,
    next,
    close: async () => {
      await happy.happyDOM.abort();
      await happy.happyDOM.close();
      restorePrototypes();
    },
  };
}

/** The `data` message for a request: every dataset's result. */
export function dataMessage(
  results: readonly DatasetResult[],
  generation = 1,
): { tq: number; type: string; generation: number; datasets: unknown[] } {
  return {
    tq: PROTOCOL,
    type: "data",
    generation,
    datasets: results.map((result) => ({ status: "ok", result })),
  };
}

/** The `init` message for a request. */
export function initMessage(request: CheckRequest, mode: "check" | "view" = "check") {
  return {
    tq: PROTOCOL,
    type: "init",
    runtime: request.runtime,
    page: request.page,
    spec: request.spec,
    locale: request.locale,
    tz: request.tz,
    theme: "light",
    mode,
  };
}

const failed = (problem: Problem): CheckVerdict => ({ verdict: "failed", problems: [problem] });

/**
 * The verdict on `request` as far as a DOM shows it: the runtime's `error` is `script_error`, no
 * `returned` within the render time limit is `not_rendered`, no `rendered` after it is a render
 * `timeout`, and then the comparison judges the document with DOM functions captured before the
 * page ran. Errors the runtime cannot catch, navigation, requests and real layout need a browser.
 */
export async function checkInFrame(request: CheckRequest): Promise<CheckVerdict> {
  const frame = await openFrame();
  try {
    frame.runtime.receive(initMessage(request));
    frame.runtime.receive(dataMessage(request.datasets));
    const scriptError = (message: SandboxMessage) =>
      message.type === "error"
        ? failed(
            message.message === undefined
              ? { kind: "script_error" }
              : { kind: "script_error", message: message.message },
          )
        : null;
    const first = await frame.next(
      (message) => message.type === "error" || message.type === "returned",
      RENDER_TIME_LIMIT_MS,
    );
    if (first === null) return failed({ kind: "not_rendered" });
    const firstError = scriptError(first);
    if (firstError !== null) return firstError;
    const done = await frame.next(
      (message) => message.type === "error" || message.type === "rendered",
      RENDER_TIME_LIMIT_MS + 4 * QUIET_PERIOD_MS,
      frame.messages.indexOf(first) + 1,
    );
    if (done === null) return failed({ kind: "timeout", phase: "render" });
    const doneError = scriptError(done);
    if (doneError !== null) return doneError;
    return verdictOf(
      compareDocument(
        frame.dom,
        { spec: request.spec, results: request.datasets, tz: request.tz },
        request.locale,
        false,
      ),
    );
  } finally {
    await frame.close();
  }
}
