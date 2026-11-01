// The sandbox runtime (frontend/sandbox/README.md): `window.tq`, the page loader and the sandbox
// end of the postMessage protocol. It runs in the page's own realm, so nothing it holds is secret
// and nothing it reports is trusted: the host treats its messages as the page's, and the checker
// compares from an isolated world with its own data. What the runtime does guard is its own
// working: it captures the functions it relies on before any page script runs, so a page that
// replaces timers, observers or DOM prototypes later cannot steer what the runtime reads or when
// it reports.
import { compareDocument, type Fix, withoutNumbers } from "./compare.ts";
import { captureDom, type DomAccess, type DomWindow } from "./dom.ts";
import { type DisplayValue, formatValue } from "./format.ts";
import { harden } from "./harden.ts";
import {
  COLUMN_NAME_PATTERN,
  DataMessageSchema,
  type DatasetResult,
  InitMessageSchema,
  MARK_CHANNELS,
  MAX_FRAME_HEIGHT,
  NAME_PATTERN,
  PROTOCOL,
  type PublicSpec,
  parseReference,
  QUIET_PERIOD_MS,
  RENDER_TIME_LIMIT_MS,
  type SandboxMessage,
} from "./protocol.ts";
import { visibleMessage } from "./text.ts";
import { tickTimeText } from "./time.ts";
import { cellDisplay, type Model, resolve, resultOf } from "./values.ts";

type InitMessage = ReturnType<typeof InitMessageSchema.parse>;
type DataMessage = ReturnType<typeof DataMessageSchema.parse>;

/** What the runtime needs from its surroundings; the entry point (entries/runtime.ts) passes the real ones. */
export interface RuntimeEnvironment {
  /** The sandbox's window: its document is where the page draws. */
  window: DomWindow;
  /** Sends a message to the host. */
  post(message: SandboxMessage): void;
  /** The d3 namespace the page gets as `d3`. */
  d3: unknown;
  /**
   * Whether the window lays its document out: true in a browser (installSandbox), false in a test
   * DOM without layout. The display-time comparison's layout rules run only with it, and it is
   * never read from the page (compareDocument).
   */
  layout: boolean;
}

export interface Runtime {
  /** Handles a message from the host (already known to come from the parent window). */
  receive(data: unknown): void;
}

const apply = Reflect.apply;

const dateTime = Date.prototype.getTime;

/**
 * The time of a Date from any realm (a page's d3 makes its own), or `undefined` for anything
 * else, including an object that only inherits from Date.prototype.
 */
export function timeOf(value: unknown): number | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  try {
    return apply(dateTime, value, []) as number;
  } catch {
    return undefined;
  }
}

/** The attributes the display-time comparison sets (sandbox.css styles them). */
const FLAGS = ["data-tq-flag", "data-tq-flag-pseudo", "data-tq-flag-hide"] as const;

/**
 * The longest a page can postpone a display-time recheck by changing the DOM: however fast it keeps
 * mutating, the comparison runs again within this long of the first change of a burst, so a value
 * the page wrote after the last comparison is corrected within it (frontend/sandbox/README.md).
 */
export const MAX_RECHECK_DELAY_MS = 1000;

/**
 * After more than this many comparisons in a row that each had to rewrite a value the page showed,
 * the runtime hides the page, within the page's own document, which the page's scripts can reach
 * (enforcing it is Laysi/trustiot-v2#462): the runtime does not keep correcting a page that fights
 * it. Rechecks and data refreshes count alike, so a page that draws a value it cannot back on every
 * refresh reaches it too; only a comparison with nothing to rewrite starts the count again, so a
 * page that keeps updating with correct values never reaches it.
 */
export const MAX_CORRECTION_ROUNDS = 10;

/** A thrown value as an `error` message carries it: at most 200 visible characters, or nothing. */
export function describeError(error: unknown): string | undefined {
  let text: string;
  try {
    if (typeof error === "object" && error !== null && typeof (error as Error).message === "string") {
      const name = typeof (error as Error).name === "string" ? (error as Error).name : "Error";
      text = `${name}: ${(error as Error).message}`;
    } else {
      text = String(error);
    }
  } catch {
    return undefined;
  }
  return visibleMessage(text);
}

/** The page's classic scripts: no `type` or a JavaScript type, and no `src`. */
function isClassicScript(dom: DomAccess, element: Element): boolean {
  if (dom.namespaceURI(element) !== "http://www.w3.org/1999/xhtml") return false;
  if (dom.getAttribute(element, "src") !== null) return false;
  const type = dom.getAttribute(element, "type");
  return type === null || type.trim() === "" || type.trim().toLowerCase() === "text/javascript";
}

interface Delivered {
  model: Model;
  /** Datasets that failed, by name: the message key. */
  failed: Record<string, string>;
}

/** Starts the runtime in `env.window`: reports `ready` and waits for the host's `init`. */
export function startRuntime(env: RuntimeEnvironment): Runtime {
  const win = env.window;
  const doc = win.document;
  const dom = captureDom(win);
  // Everything the runtime calls later, captured before a page script can replace it.
  const FunctionConstructor = win.Function;
  const DOMParserConstructor = win.DOMParser;
  const MutationObserverConstructor = win.MutationObserver;
  const ResizeObserverConstructor = win.ResizeObserver as typeof ResizeObserver | undefined;
  const PromiseConstructor = win.Promise;
  const promiseResolve = win.Promise.resolve;
  const promiseThen = win.Promise.prototype.then;
  const setTimer = win.setTimeout;
  const clearTimer = win.clearTimeout;
  const nextFrame = win.requestAnimationFrame;
  const performanceNow = win.performance.now;
  const performance = win.performance;
  const observe = win.MutationObserver.prototype.observe;
  const disconnect = win.MutationObserver.prototype.disconnect;
  const takeRecords = win.MutationObserver.prototype.takeRecords;
  const addEventListener = win.EventTarget.prototype.addEventListener;
  const getElementById = win.Document.prototype.getElementById;
  const createElement = win.Document.prototype.createElement;
  const importNode = win.Document.prototype.importNode;
  const appendChild = win.Node.prototype.appendChild;
  const removeChild = win.Node.prototype.removeChild;
  const parseFromString = win.DOMParser.prototype.parseFromString;
  const getElementsByTagName = win.Document.prototype.getElementsByTagName;
  const defineProperty = Object.defineProperty;
  const freeze = Object.freeze;
  const d3 = env.d3 as { select?: (node: Element) => unknown } | undefined;
  const d3Select = d3?.select;

  const now = () => apply(performanceNow, performance, []) as number;
  const later = (callback: () => void, ms: number) => apply(setTimer, win, [callback, ms]) as number;
  const cancel = (timer: number | undefined) => {
    if (timer !== undefined) apply(clearTimer, win, [timer]);
  };
  const frame = (callback: () => void) => {
    if (typeof nextFrame === "function") apply(nextFrame, win, [callback]);
    else later(callback, 16);
  };
  const post = (message: SandboxMessage) => env.post(message);

  let init: InitMessage | null = null;
  let delivered: Delivered | null = null;
  let generation = 0;
  let render: ((tq: unknown) => unknown) | null = null;
  let pageApi: unknown = null;
  let renderedGeneration = 0;
  /** Counts render calls: a later call (new data, or a new render function) supersedes the earlier ones. */
  let attempt = 0;
  /** The render function the latest call used. */
  let drawnWith: unknown = null;
  /** A render of the current data is scheduled for a render function registered after it came. */
  let redrawScheduled = false;
  /** The latest render's wait for quiet, stopped as soon as a newer render starts. */
  let settling: { observer: MutationObserver; timer: number | undefined; limit: number | undefined } | null = null;
  let lastHeight = -1;
  let watcher: MutationObserver | null = null;
  let recheck: number | undefined;
  /** When a burst of page changes began; the forced recheck runs within MAX_RECHECK_DELAY_MS of it. */
  let recheckBy = 0;
  /** Consecutive comparisons (rechecks and data refreshes) that each had to rewrite a shown value. */
  let correctionStreak = 0;
  /** The elements the last display-time comparison flagged. */
  let flagged: Element[] = [];
  /** The display-time comparison can no longer show the page safely: it is hidden and not compared again. */
  let comparisonFailed = false;

  // --- the page API ------------------------------------------------------------------------

  const requireData = (what: string): Delivered => {
    if (delivered === null) throw new Error(`${what}: no data yet; call it from the render function`);
    return delivered;
  };

  const isElement = (value: unknown): value is Element => {
    if (typeof value !== "object" || value === null) return false;
    try {
      return dom.nodeType(value as Node) === 1;
    } catch {
      return false;
    }
  };

  const isSelection = (
    value: unknown,
  ): value is { each: (fn: (this: Element, d: unknown, i: number) => void) => unknown } =>
    typeof value === "object" &&
    value !== null &&
    typeof (value as { each?: unknown }).each === "function" &&
    typeof (value as { node?: unknown }).node === "function";

  /** Calls `fn` for each element of `target` (an element or a d3 selection) with its reference. */
  const eachTarget = (
    api: string,
    target: unknown,
    reference: unknown,
    fn: (element: Element, reference: string) => void,
  ) => {
    const referenceFor = (datum: unknown, index: number): string => {
      const text =
        typeof reference === "function" ? (reference as (d: unknown, i: number) => unknown)(datum, index) : reference;
      if (typeof text !== "string" || parseReference(text) === null) {
        throw new TypeError(`${api}: not a reference: ${String(text).slice(0, 80)}`);
      }
      return text;
    };
    if (isElement(target)) {
      if (typeof reference !== "string") throw new TypeError(`${api}: give an element a reference string`);
      fn(target, referenceFor(undefined, 0));
    } else if (isSelection(target)) {
      target.each(function (this: Element, datum: unknown, index: number) {
        if (isElement(this)) fn(this, referenceFor(datum, index));
      });
    } else {
      throw new TypeError(`${api}: the target is not an element or a d3 selection`);
    }
  };

  const valueFor = (reference: unknown): DisplayValue => {
    if (typeof reference !== "string") throw new TypeError("tq.value: a reference is a string");
    const parsed = parseReference(reference);
    if (parsed === null) throw new TypeError(`tq.value: not a reference: ${reference.slice(0, 80)}`);
    const resolved = resolve(requireData("tq.value").model, parsed);
    if (resolved === null) return null;
    return resolved.display instanceof Date ? new Date(resolved.display.getTime()) : resolved.display;
  };

  const formatOf = (value: unknown, format: unknown): string => {
    if (format !== undefined && format !== null && typeof format !== "string") {
      throw new TypeError("tq.format: a format is a string");
    }
    const time = timeOf(value);
    const shown =
      value === undefined
        ? null
        : value === null || typeof value === "number" || typeof value === "string" || typeof value === "boolean"
          ? value
          : time !== undefined
            ? new Date(time)
            : undefined;
    if (shown === undefined) throw new TypeError("tq.format: a value is a number, text, a boolean, a Date or null");
    return formatValue(shown, format ?? undefined, (init as InitMessage).locale);
  };

  const datasetOf = (name: unknown) => {
    const { model, failed } = requireData("tq.dataset");
    const declared = model.spec.datasets.find((dataset) => dataset.name === name);
    if (declared === undefined) throw new Error(`tq.dataset: no dataset named ${String(name).slice(0, 60)}`);
    const result = resultOf(model, declared.name);
    const base = {
      name: declared.name,
      title: declared.title,
      description: declared.description,
      source: declared.source,
    };
    if (result === undefined) {
      return {
        ...base,
        status: "error",
        messageKey: failed[declared.name] ?? "",
        columns: [],
        rows: [],
        rowCount: 0,
        meta: { queried_at: null, row_count: 0, range_from: null, range_to: null },
      };
    }
    const meta = (field: "queried_at" | "range_from" | "range_to") => {
      const reference = parseReference(`m|${declared.name}|${field}`);
      const resolved = reference === null ? null : resolve(model, reference);
      return resolved?.display instanceof Date ? resolved.display : null;
    };
    return {
      ...base,
      status: "ok",
      columns: result.columns.map((column) => ({ name: column.name, type: column.type })),
      rows: rowsOf(result),
      rowCount: result.row_count,
      meta: {
        queried_at: meta("queried_at"),
        row_count: result.row_count,
        range_from: meta("range_from"),
        range_to: meta("range_to"),
      },
    };
  };

  const rowsOf = (result: DatasetResult) =>
    result.rows.map((cells, index) => {
      const row: Record<string, unknown> = {};
      result.columns.forEach((column, i) => {
        // defineProperty, so a column named __proto__ is a column and not the prototype.
        defineProperty(row, column.name, {
          value: cellDisplay(column.type, cells[i] ?? null),
          enumerable: true,
          writable: true,
          configurable: true,
        });
      });
      defineProperty(row, "$row", { value: index, enumerable: true, writable: true, configurable: true });
      return row;
    });

  const setMark = (element: Element, reference: string, attributes: Record<string, string | null>) => {
    dom.setAttribute(element, "data-tq", reference);
    for (const [name, value] of Object.entries(attributes)) {
      if (value === null) dom.removeAttribute(element, name);
      else dom.setAttribute(element, name, value);
    }
  };

  const textOf = (target: unknown, reference: unknown, format?: unknown) => {
    if (format !== undefined && format !== null && typeof format !== "string") {
      throw new TypeError("tq.text: a format is a string");
    }
    eachTarget("tq.text", target, reference, (element, text) => {
      dom.setTextContent(element, formatOf(valueFor(text), format));
      setMark(element, text, {
        "data-tq-format": typeof format === "string" ? format : null,
        "data-tq-chart": null,
        "data-tq-channel": null,
      });
    });
    return target;
  };

  const markOf = (target: unknown, reference: unknown, options: unknown) => {
    const { chart, channel } = (options ?? {}) as { chart?: unknown; channel?: unknown };
    if (typeof chart !== "string" || !NAME_PATTERN.test(chart)) {
      throw new TypeError("tq.mark: options.chart is a name such as daily_energy");
    }
    if (typeof channel !== "string" || !MARK_CHANNELS.some((known) => known === channel)) {
      throw new TypeError(`tq.mark: options.channel is one of ${MARK_CHANNELS.join(", ")}`);
    }
    eachTarget("tq.mark", target, reference, (element, text) => {
      setMark(element, text, { "data-tq-chart": chart, "data-tq-channel": channel, "data-tq-format": null });
    });
    return target;
  };

  const decorOf = (target: unknown) => {
    if (isElement(target)) dom.setAttribute(target, "data-tq-decor", "");
    else if (isSelection(target)) {
      target.each(function (this: Element) {
        if (isElement(this)) dom.setAttribute(this, "data-tq-decor", "");
      });
    } else throw new TypeError("tq.decor: the target is not an element or a d3 selection");
    return target;
  };

  const axisOf = (target: unknown, axis: unknown, options: unknown) => {
    const { dataset, column, format } = (options ?? {}) as { dataset?: unknown; column?: unknown; format?: unknown };
    if (typeof dataset !== "string" || typeof column !== "string" || !COLUMN_NAME_PATTERN.test(column)) {
      throw new TypeError("tq.axis: options are {dataset, column, format?}");
    }
    if (format !== undefined && typeof format !== "string") throw new TypeError("tq.axis: options.format is a string");
    const { model } = requireData("tq.axis");
    const result = resultOf(model, dataset);
    const columnIndex = result?.columns.findIndex((candidate) => candidate.name === column) ?? -1;
    if (result === undefined || columnIndex < 0) {
      throw new Error(`tq.axis: dataset ${dataset.slice(0, 60)} has no column ${column}`);
    }
    const d3Axis = axis as {
      (selection: unknown): void;
      scale?: () => { bandwidth?: unknown };
      tickFormat?: (format: (value: unknown) => string) => unknown;
    };
    if (typeof d3Axis !== "function" || typeof d3Axis.scale !== "function" || typeof d3Axis.tickFormat !== "function") {
      throw new TypeError("tq.axis: the axis is a d3 axis such as d3.axisBottom(scale)");
    }
    const band = typeof d3Axis.scale().bandwidth === "function";
    d3Axis.tickFormat((value) => formatOf(value, format));
    const groups: Element[] = [];
    if (isElement(target)) groups.push(target);
    else if (isSelection(target)) {
      target.each(function (this: Element) {
        if (isElement(this)) groups.push(this);
      });
    } else throw new TypeError("tq.axis: the target is an SVG <g> or a d3 selection of one");
    const type = result.columns[columnIndex]?.type;
    const categories = band ? result.rows.map((row) => cellDisplay(type, row[columnIndex] ?? null)) : [];
    const same = (a: unknown, b: unknown) => {
      const [timeA, timeB] = [timeOf(a), timeOf(b)];
      return timeA !== undefined || timeB !== undefined ? timeA === timeB : a === b;
    };
    for (const group of groups) {
      if (typeof d3Select !== "function") throw new Error("tq.axis: d3 is not loaded");
      d3Axis(apply(d3Select, d3, [group]));
      dom.setAttribute(group, "data-tq-axis", `${dataset}|${column}`);
      for (let tick = dom.firstChild(group); tick !== null; tick = dom.nextSibling(tick)) {
        if (!isElement(tick) || !` ${dom.getAttribute(tick, "class") ?? ""} `.includes(" tick ")) continue;
        let text: Element | null = null;
        for (let child = dom.firstChild(tick); child !== null; child = dom.nextSibling(child)) {
          if (isElement(child) && dom.localName(child) === "text") text = child;
        }
        if (text === null) continue;
        const value = (tick as unknown as { __data__?: unknown }).__data__;
        let reference: string | null = null;
        if (band) {
          const row = categories.findIndex((category) => same(category, value));
          reference = row < 0 ? null : `c|${dataset}|${column}|${row}`;
        } else if (typeof value === "number") {
          reference = `t|${dataset}|${column}|${String(value)}`;
        } else if (timeOf(value) !== undefined) {
          const time = tickTimeText(new Date(timeOf(value) as number));
          reference = time === null ? null : `t|${dataset}|${column}|${time}`;
        }
        if (reference === null || parseReference(reference) === null) {
          for (const name of ["data-tq", "data-tq-format", "data-tq-chart", "data-tq-channel"])
            dom.removeAttribute(text, name);
          continue;
        }
        setMark(text, reference, {
          "data-tq-format": typeof format === "string" ? format : null,
          "data-tq-chart": null,
          "data-tq-channel": null,
        });
      }
    }
    return target;
  };

  const makeTq = (message: InitMessage) =>
    freeze({
      on(event: unknown, handler: unknown) {
        if (event !== "data") throw new TypeError('tq.on: the only event is "data"');
        if (typeof handler !== "function") throw new TypeError("tq.on: the handler is a function");
        render = handler as (tq: unknown) => unknown;
        // Registered after the data came (from a timer, say, or replacing the function that drew
        // it): draw the current data with it. Registering the function that drew it again, as a
        // render that registers itself does, draws nothing more.
        if (delivered !== null && !redrawScheduled && (renderedGeneration < generation || handler !== drawnWith)) {
          redrawScheduled = true;
          later(() => {
            redrawScheduled = false;
            // New data may have been drawn with this function in the meantime.
            if (renderedGeneration < generation || render !== drawnWith) startRender(generation);
          }, 0);
        }
      },
      dataset: datasetOf,
      value: valueFor,
      text: textOf,
      mark: markOf,
      axis: axisOf,
      decor: decorOf,
      format: formatOf,
      locale: message.locale,
      tz: message.tz,
      theme: message.theme,
    });

  // --- loading the page --------------------------------------------------------------------

  const rootElement = (): Element => {
    const found = apply(getElementById, doc, ["tq-root"]) as Element | null;
    if (found !== null) return found;
    const created = apply(createElement, doc, ["div"]) as Element;
    dom.setAttribute(created, "id", "tq-root");
    const body = doc.body;
    apply(appendChild, body, [created]);
    return created;
  };

  const loadPage = (message: InitMessage, tq: unknown) => {
    const parsed = apply(parseFromString, new DOMParserConstructor(), [message.page, "text/html"]) as Document;
    const scripts: string[] = [];
    const found = apply(getElementsByTagName, parsed, ["script"]) as HTMLCollectionOf<Element>;
    const elements: Element[] = [];
    for (let i = 0; i < found.length; i++) {
      const element = found[i];
      if (element !== undefined) elements.push(element);
    }
    for (const element of elements) {
      if (isClassicScript(dom, element)) scripts.push(dom.textContent(element));
      const parent = dom.parentNode(element);
      if (parent !== null) apply(removeChild, parent, [element]);
    }
    const root = rootElement();
    for (const container of [parsed.head, parsed.body]) {
      if (container === null) continue;
      for (let node = dom.firstChild(container); node !== null; node = dom.firstChild(container)) {
        const copy = apply(importNode, doc, [node, true]) as Node;
        apply(removeChild, container, [node]);
        apply(appendChild, root, [copy]);
      }
    }
    // The sandbox document's CSP is `script-src 'self' 'unsafe-eval'` (`SANDBOX_CSP` in
    // crates/contract/src/api/dashboard/sandbox.rs, whose module documentation says why), so
    // `new Function` is allowed there and in no other document.
    for (const source of scripts) {
      let run: (tq: unknown, d3: unknown) => unknown;
      try {
        run = new FunctionConstructor("tq", "d3", `"use strict";\n${source}`) as typeof run;
        apply(run, undefined, [tq, env.d3]);
      } catch (error) {
        post({ tq: PROTOCOL, type: "error", phase: "page", ...messageOf(error) });
        return;
      }
    }
  };

  const messageOf = (error: unknown): { message?: string } => {
    const message = describeError(error);
    return message === undefined ? {} : { message };
  };

  // --- rendering and the render-finished signal ---------------------------------------------

  const startRender = (current: number) => {
    if (render === null || delivered === null || current !== generation) return;
    renderedGeneration = current;
    const mine = ++attempt;
    stopSettling();
    drawnWith = render;
    const started = now();
    let returned: unknown;
    try {
      returned = apply(render, undefined, [pageApi]);
    } catch (error) {
      post({ tq: PROTOCOL, type: "error", phase: "render", ...messageOf(error) });
      return;
    }
    const finish = () => {
      if (mine !== attempt) return;
      post({ tq: PROTOCOL, type: "returned", generation: current });
      settle(current, mine, started);
    };
    const fail = (error: unknown) => {
      if (mine !== attempt) return;
      post({ tq: PROTOCOL, type: "error", phase: "render", ...messageOf(error) });
    };
    if ((typeof returned !== "object" || returned === null) && typeof returned !== "function") {
      finish();
      return;
    }
    // Any object may be a thenable, and reading its `then` (or a promise's `constructor`) runs
    // the page's code, which may throw: Promise.resolve assimilates it, and whatever throws on
    // the way is the render's failure.
    try {
      const promise = apply(promiseResolve, PromiseConstructor, [returned]) as Promise<unknown>;
      apply(promiseThen, promise, [finish, fail]);
    } catch (error) {
      fail(error);
    }
  };

  /** Ends the current wait for quiet: its observer and timers. */
  const stopSettling = () => {
    if (settling === null) return;
    apply(disconnect, settling.observer, []);
    cancel(settling.timer);
    cancel(settling.limit);
    settling = null;
  };

  /**
   * Two frames, then QUIET_PERIOD_MS without a change; in check mode within the time limit. View
   * mode has no deadline to finish, but the first comparison still runs within MAX_RECHECK_DELAY_MS
   * even if the page never goes quiet, so a page that keeps mutating cannot put off being compared;
   * the recheck watcher takes over from there. Only the latest render waits: a newer one stops this
   * wait (stopSettling) before it starts its own.
   */
  const settle = (current: number, mine: number, started: number) => {
    const checking = init?.mode === "check";
    frame(() =>
      frame(() => {
        if (mine !== attempt) return;
        const quiet = () => {
          if (mine !== attempt) return;
          stopSettling();
          if (checking && now() - started > RENDER_TIME_LIMIT_MS) return;
          if (!checking) showChecked();
          post({ tq: PROTOCOL, type: "rendered", generation: current });
          reportHeight();
        };
        const wait: NonNullable<typeof settling> = {
          observer: new MutationObserverConstructor(() => {
            if (settling !== wait) return;
            cancel(wait.timer);
            wait.timer = later(quiet, QUIET_PERIOD_MS);
          }),
          timer: undefined,
          limit: undefined,
        };
        stopSettling();
        settling = wait;
        apply(observe, wait.observer, [doc, { subtree: true, childList: true, attributes: true, characterData: true }]);
        wait.timer = later(quiet, QUIET_PERIOD_MS);
        if (checking) {
          // Past the time limit nothing is reported: stop waiting then.
          wait.limit = later(
            () => {
              if (settling === wait) stopSettling();
            },
            Math.max(0, RENDER_TIME_LIMIT_MS - (now() - started)) + 1,
          );
        } else {
          // The page never settling must not keep it from being compared: force the comparison.
          wait.limit = later(() => {
            if (settling === wait) quiet();
          }, MAX_RECHECK_DELAY_MS);
        }
      }),
    );
  };

  // --- display time: compare and show only what matches ------------------------------------

  const flag = (element: Element, name: (typeof FLAGS)[number]) => {
    dom.setAttribute(element, name, "");
    flagged.push(element);
  };

  /** Applies a fix; returns whether it rewrote a value the page had shown (text, a number, not a flag). */
  const applyFix = (fix: Fix): boolean => {
    switch (fix.kind) {
      case "text": {
        const rewrote = dom.textContent(fix.element) !== "\u2014";
        if (rewrote) dom.setTextContent(fix.element, "\u2014");
        flag(fix.element, "data-tq-flag");
        return rewrote;
      }
      case "graphic":
        flag(fix.element, "data-tq-flag");
        return false;
      case "data": {
        const text = dom.data(fix.node);
        const replaced = withoutNumbers(text);
        const rewrote = replaced !== text;
        if (rewrote) dom.setData(fix.node, replaced);
        if (fix.element !== null) flag(fix.element, "data-tq-flag");
        return rewrote;
      }
      case "attribute": {
        const value = dom.getAttribute(fix.element, fix.name);
        const replaced = value === null ? null : withoutNumbers(value);
        if (value !== null && replaced !== value) {
          dom.setAttribute(fix.element, fix.name, replaced as string);
          return true;
        }
        return false;
      }
      case "pseudo":
        flag(fix.element, "data-tq-flag-pseudo");
        return false;
      case "hide":
        flag(fix.element, "data-tq-flag-hide");
        return false;
    }
  };

  /** Hides the whole page, within its own document (enforcing it is Laysi/trustiot-v2#462). */
  const hideForGood = () => {
    comparisonFailed = true;
    cancel(recheck);
    if (watcher !== null) apply(disconnect, watcher, []);
    const root = dom.documentElement();
    if (root !== null) dom.setAttribute(root, "data-tq-flag-hide", "");
  };

  /**
   * The display-time comparison. It runs in the page's realm; the realm's built-ins are frozen
   * (harden.ts) and the DOM functions were captured before the page ran (dom.ts), so replacing them
   * does not change what it computes from a given DOM. What it does with the result (corrected text,
   * flags, the hidden document) lives in the page's own document: it corrects and flags there, and
   * is not an enforcement boundary; enforcing the outcome is the host page's job
   * (Laysi/trustiot-v2#462). If it throws, the document is hidden (`hideForGood`).
   */
  const showChecked = () => {
    if (comparisonFailed) return;
    try {
      compareAndShow();
    } catch {
      hideForGood();
    }
  };

  /**
   * Schedules the next recheck after a page change: normally once the DOM is quiet (QUIET_PERIOD_MS
   * after the last change), but never later than MAX_RECHECK_DELAY_MS after the first change of the
   * burst, so a page that keeps mutating cannot postpone the comparison indefinitely.
   */
  const scheduleRecheck = () => {
    if (comparisonFailed) return;
    const nowMs = now();
    if (recheck === undefined) recheckBy = nowMs + MAX_RECHECK_DELAY_MS;
    cancel(recheck);
    recheck = later(
      () => {
        recheck = undefined;
        if (watcher !== null) apply(takeRecords, watcher, []);
        showChecked();
      },
      Math.max(0, Math.min(QUIET_PERIOD_MS, recheckBy - nowMs)),
    );
  };

  const compareAndShow = () => {
    if (delivered === null || init === null) return;
    // Flags describe this comparison only: a value a later render corrected is shown plainly
    // again. Text the runtime replaced stays replaced until the page writes it again.
    for (const element of flagged) {
      for (const name of FLAGS) if (dom.getAttribute(element, name) !== null) dom.removeAttribute(element, name);
    }
    flagged = [];
    const { findings } = compareDocument(dom, delivered.model, init.locale, env.layout);
    let rewritten = 0;
    for (const finding of findings) {
      if (finding.fix !== null) {
        if (applyFix(finding.fix)) rewritten++;
      } else if (finding.problem.kind === "too_large") {
        const root = dom.documentElement();
        if (root !== null) flag(root, "data-tq-flag-hide");
      }
    }
    // A page whose comparisons keep having to rewrite a value (put back after a correction, or drawn
    // again on each refresh) is hidden; one that settles, or keeps updating
    // with correct values (nothing to rewrite), is not.
    if (rewritten > 0) {
      if (++correctionStreak > MAX_CORRECTION_ROUNDS) {
        hideForGood();
        return;
      }
    } else {
      correctionStreak = 0;
    }
    // Changes after the render (a tooltip, a timer) are compared again once they settle, within
    // the bound; the runtime's own changes above are taken out so they are not a reason to.
    if (watcher === null) {
      watcher = new MutationObserverConstructor(() => scheduleRecheck());
      apply(observe, watcher, [doc, { subtree: true, childList: true, attributes: true, characterData: true }]);
    }
    apply(takeRecords, watcher, []);
  };

  // --- the host's messages -------------------------------------------------------------------

  const reportHeight = () => {
    const root = dom.documentElement();
    if (root === null) return;
    const box = dom.box(root);
    const scroll = (root as unknown as { scrollHeight?: unknown }).scrollHeight;
    const height = Math.min(
      MAX_FRAME_HEIGHT,
      Math.max(0, Math.ceil(Math.max(box.height, typeof scroll === "number" && Number.isFinite(scroll) ? scroll : 0))),
    );
    if (height !== lastHeight) {
      lastHeight = height;
      post({ tq: PROTOCOL, type: "resize", height });
    }
  };

  const start = (message: InitMessage) => {
    init = message;
    const root = dom.documentElement();
    if (root !== null) {
      dom.setAttribute(root, "lang", message.locale);
      dom.setAttribute(root, "data-theme", message.theme);
    }
    const tq = makeTq(message);
    pageApi = tq;
    // The page may shadow or replace window.tq; the runtime keeps its own reference.
    defineProperty(win, "tq", { value: tq, writable: false, enumerable: true, configurable: true });
    if (ResizeObserverConstructor !== undefined && root !== null) {
      const resizes = new ResizeObserverConstructor(() => reportHeight());
      resizes.observe(root);
    }
    if (message.mode === "view") {
      apply(addEventListener, doc, [
        "click",
        (event: Event) => {
          for (let node = event.target as Node | null; node !== null; node = dom.parentNode(node)) {
            if (!isElement(node)) continue;
            const text = dom.getAttribute(node, "data-tq");
            if (text === null) continue;
            const reference = parseReference(text);
            if (reference !== null && delivered !== null && resolve(delivered.model, reference) !== null) {
              post({ tq: PROTOCOL, type: "source", reference: text });
            }
            return;
          }
        },
        true,
      ]);
    }
    loadPage(message, tq);
  };

  const deliver = (message: DataMessage, spec: PublicSpec) => {
    // Exactly one entry per declared dataset, or the message is not one the host sends.
    const names = message.datasets.map((entry) => (entry.status === "ok" ? entry.result.dataset : entry.dataset));
    const declared = spec.datasets.map((dataset) => dataset.name);
    if (names.length !== declared.length || declared.some((name) => names.filter((n) => n === name).length !== 1)) {
      return;
    }
    const failed: Record<string, string> = Object.create(null);
    const results: DatasetResult[] = [];
    for (const entry of message.datasets) {
      if (entry.status === "ok") results.push(entry.result);
      else failed[entry.dataset] = entry.message_key;
    }
    delivered = { model: { spec, results, tz: (init as InitMessage).tz }, failed };
    generation = message.generation;
    startRender(generation);
  };

  const receive = (data: unknown) => {
    if (init === null) {
      const parsed = InitMessageSchema.safeParse(data);
      if (parsed.success) start(parsed.data);
      return;
    }
    const parsed = DataMessageSchema.safeParse(data);
    if (parsed.success && parsed.data.generation > generation) deliver(parsed.data, init.spec);
  };

  // Errors the runtime cannot catch itself: in view mode the host learns that the page failed.
  // The browser hides their text from an opaque origin ("Script error."), so the message is left
  // out; the checker reads them, with their text, through the DevTools protocol instead.
  const asyncError = () => {
    if (init !== null && init.mode === "view") post({ tq: PROTOCOL, type: "error", phase: "render" });
  };
  apply(addEventListener, win, ["error", asyncError]);
  apply(addEventListener, win, ["unhandledrejection", asyncError]);

  post({ tq: PROTOCOL, type: "ready", runtime: 1 });
  return { receive };
}

/**
 * Reads an event's `isTrusted` through the getter the window has now, before any page script
 * runs; `null` when the DOM has none. Browsers define `isTrusted` on every event as a
 * non-configurable own accessor (WebIDL `[LegacyUnforgeable]`), so a page cannot change it on an
 * event, and the getter reads the event it is called on; capturing it also keeps a page's
 * replacement of anything along the way out of the answer.
 */
export function trustedEvents(win: DomWindow): ((event: Event) => boolean) | null {
  let get: (() => unknown) | undefined;
  try {
    get = Object.getOwnPropertyDescriptor(new win.Event("tq-probe"), "isTrusted")?.get;
    for (let prototype: object | null = win.Event.prototype; get === undefined && prototype !== null; ) {
      get = Object.getOwnPropertyDescriptor(prototype, "isTrusted")?.get;
      prototype = Object.getPrototypeOf(prototype) as object | null;
    }
  } catch {
    return null;
  }
  if (get === undefined) return null;
  const getter = get;
  return (event) => {
    try {
      return apply(getter, event, []) === true;
    } catch {
      return false;
    }
  };
}

/**
 * Installs the runtime in the sandbox document: only in an opaque-origin frame (the sandbox's CSP
 * makes it one), so the files stay inert anywhere else, and only for messages the parent window
 * itself posted.
 */
export function installSandbox(win: DomWindow): Runtime | null {
  const parent = win.parent;
  if (win.origin !== "null" || parent === win || parent === null) return null;
  const postMessage = parent.postMessage;
  const isTrusted = trustedEvents(win);
  if (isTrusted === null) return null;
  const source = Object.getOwnPropertyDescriptor(win.MessageEvent.prototype, "source")?.get;
  const data = Object.getOwnPropertyDescriptor(win.MessageEvent.prototype, "data")?.get;
  if (source === undefined || data === undefined) return null;
  // Freeze the realm's built-ins before the first page script runs (on `init`, which arrives after
  // this returns): the display-time comparison and the formatters it calls then read genuine
  // intrinsics whatever the page does. The checker compares from a separate isolated world and does
  // not rely on this; it hardens here too so a page runs under the same realm in both modes.
  harden();
  const runtime = startRuntime({
    window: win,
    post: (message) => apply(postMessage, parent, [message, "*"]),
    d3: (win as unknown as { d3?: unknown }).d3,
    // The sandbox frame is a browser document, which is laid out.
    layout: true,
  });
  apply(win.EventTarget.prototype.addEventListener, win, [
    "message",
    (event: MessageEvent) => {
      if (isTrusted(event) && apply(source, event, []) === parent) runtime.receive(apply(data, event, []));
    },
  ]);
  return runtime;
}
