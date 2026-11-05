// The sandbox runtime (frontend/sandbox/README.md): `window.tq`, the page loader and the sandbox
// end of the postMessage protocol. It runs in the page's own realm, so nothing it holds is secret
// and nothing it reports is trusted: the host treats its messages as the page's, and the checker
// compares from an isolated world with its own data. What the runtime does guard is its own
// working: it captures the functions it relies on before any page script runs, so a page that
// replaces timers, observers or DOM prototypes later cannot steer what the runtime reads or when
// it reports.
//
// In display mode the runtime also reports to the host over a MessagePort the host hands it with
// `init`, before any page script runs (README, 「顯示時比對的強制」): the result of every comparison,
// a heartbeat, and the page's errors, size and source clicks. The page cannot reach the port, so
// the host shows the frame only on what arrives there. It compares after every change it is told
// of (a DOM mutation, a change through an interface `seal.ts` wraps, an event in `POKES`), and also
// at random times (`sample`): a comparison at a random time that finds something no such change
// explains means the page changed what it shows by a way the runtime is not told of, and the
// runtime gives up on the page for good (`failed` with `unnotified`).
import { compareDocument, type Fix, withoutNumbers } from "./compare.ts";
import { captureDom, type DomAccess, type DomWindow } from "./dom.ts";
import { type DisplayValue, formatValue } from "./format.ts";
import { harden } from "./harden.ts";
import {
  COLUMN_NAME_PATTERN,
  DataMessageSchema,
  type DatasetResult,
  HEARTBEAT_INTERVAL_MS,
  InitMessageSchema,
  MARK_CHANNELS,
  MAX_FRAME_HEIGHT,
  NAME_PATTERN,
  type PortMessage,
  PROTOCOL,
  type PublicSpec,
  parseReference,
  QUIET_PERIOD_MS,
  RENDER_TIME_LIMIT_MS,
  type SandboxMessage,
  STALE_AFTER_MS,
} from "./protocol.ts";
import { seal } from "./seal.ts";
import { visibleMessage } from "./text.ts";
import { tickTimeText } from "./time.ts";
import { cellDisplay, type Model, resolve, resultOf } from "./values.ts";

type InitMessage = ReturnType<typeof InitMessageSchema.parse>;
type DataMessage = ReturnType<typeof DataMessageSchema.parse>;

/** What the runtime needs from its surroundings; the entry point (entries/runtime.ts) passes the real ones. */
export interface RuntimeEnvironment {
  /** The sandbox's window: its document is where the page draws. */
  window: DomWindow;
  /** Sends a message to the host through the window (check mode, and `ready`). */
  post(message: SandboxMessage): void;
  /** The d3 namespace the page gets as `d3`. */
  d3: unknown;
  /**
   * Whether the window lays its document out: true in a browser (installSandbox), false in a test
   * DOM without layout. The display-time comparison's layout rules run only with it, and it is
   * never read from the page (compareDocument).
   */
  layout: boolean;
  /**
   * Whether an event came from the browser rather than from a page script (`trustedEvents`): only
   * a real click asks the host for a value's source.
   */
  isTrusted(event: Event): boolean;
}

/** Sends a message to the host over the port it handed over with `init` (display mode). */
export type Reporter = (message: PortMessage) => void;

export interface Runtime {
  /**
   * Handles a message from the host (already known to come from the parent window). `report` is
   * the port that came with it; a display-mode `init` without one is ignored.
   */
  receive(data: unknown, report?: Reporter): void;
  /** The CSS object model changed a style (`seal`): compare again before the next paint. */
  styleChanged(): void;
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

/**
 * Events after which the page may look different without a DOM change (`:hover`, `:focus`,
 * `:target`, a media query, a started animation): the runtime compares in the next animation frame.
 * A custom page plays no motion (sandbox.css for CSS animations and transitions, seal.ts for the
 * Web Animations API), so a `:hover` style applies at once, and an animation is over the moment it
 * starts, which the comparison does not count (dom.ts, `ended`: only an animation with no delay,
 * active duration or end delay that plays forwards at or past 0). One that escapes
 * that (a style attribute's `!important`, a sheet a script moves ahead of sandbox.css, an
 * animation a script holds before its start) is a live
 * animation, which a comparison counts as a finding (compare.ts), so the frame stays hidden while
 * it runs; one started through the Web Animations API is reported by the seal, and the heartbeat's
 * comparison checks the state an animation ends in.
 */
const POKES = [
  "pointerover",
  "pointerout",
  "pointerdown",
  "pointerup",
  "focusin",
  "focusout",
  "focus",
  "blur",
  "keydown",
  "keyup",
  // A form control's text can change without a key (a paste from the context menu, autofill).
  "input",
  "change",
  "scroll",
  "resize",
  "hashchange",
  "popstate",
  "selectionchange",
  "toggle",
  "contentvisibilityautostatechange",
  "animationstart",
  "animationiteration",
  "animationend",
  "animationcancel",
  "transitionrun",
  "transitionstart",
  "transitionend",
  "transitioncancel",
] as const;

/**
 * Events fired before a change that follows in the same task (a popover about to open, a
 * same-document navigation about to change `:target`): the runtime compares at the end of that task,
 * before the next paint, as after a DOM mutation.
 */
const NOTICES = ["beforetoggle"] as const;

/**
 * Media features the user's environment changes without any DOM change or event the page sees on
 * its window (the system's color scheme, contrast, motion preference, input devices): a change of
 * one is a reason to compare, as a poke is. The resolution is watched separately (it changes when
 * the window moves to another screen).
 */
const MEDIA_FEATURES = [
  "(prefers-color-scheme: dark)",
  "(prefers-contrast: more)",
  "(prefers-contrast: less)",
  "(prefers-reduced-motion: reduce)",
  "(prefers-reduced-transparency: reduce)",
  "(forced-colors: active)",
  "(inverted-colors: inverted)",
  "(orientation: portrait)",
  "(hover: hover)",
  "(any-hover: hover)",
  "(pointer: fine)",
  "(any-pointer: fine)",
  "(display-mode: standalone)",
  "(dynamic-range: high)",
  "(color-gamut: p3)",
  "(color-gamut: rec2020)",
] as const;

/**
 * The mean time between two samples at random times (`sample`) while the latest comparison was
 * clean, so the host may be showing the frame, in milliseconds: SAMPLE_COST_FACTOR times what the
 * sample's two comparisons of this page cost, but at least SAMPLE_MEAN_MIN_MS and at most
 * SAMPLE_MEAN_MAX_MS (`sampleMeanMs`). The intervals are exponentially distributed (memoryless):
 * knowing when earlier samples ran tells the page nothing about when the next one runs. Each sample
 * compares around the next paint (`sampleAroundPaint`). Sampling is defence in depth, not a bound
 * (README, 「顯示時比對的強制」, with the measured times to close).
 */
export const SAMPLE_MEAN_MIN_MS = 100;
/** Comparisons at random times take about 1 / SAMPLE_COST_FACTOR of the thread's time, below SAMPLE_MEAN_MAX_MS. */
export const SAMPLE_COST_FACTOR = 20;
/**
 * The longest mean interval (32 s): the one that keeps a page whose comparison costs
 * STALE_AFTER_MS - HEARTBEAT_INTERVAL_MS (800 ms) at 1 / SAMPLE_COST_FACTOR. No report leaves the
 * frame while a comparison runs, and the last one before it can be up to a heartbeat period old, so
 * the host may hide the frame during a comparison of more than 800 ms (STALE_AFTER_MS), more likely
 * the longer it runs and every time from 1 s on, and each time it does while the frame is shown
 * counts toward the host's MAX_REGRESSIONS (guard.ts). A longer bound would spare hardly any page
 * that can be shown and would only space the samples of the costliest pages further apart (README,
 * 「顯示時比對的強制」).
 */
export const SAMPLE_MEAN_MAX_MS = 2 * SAMPLE_COST_FACTOR * (STALE_AFTER_MS - HEARTBEAT_INTERVAL_MS);
/**
 * While the latest comparison found something (the host hides the frame), sampling is this much
 * sparser, up to SAMPLE_MEAN_MAX_MS.
 */
export const SAMPLE_IDLE_FACTOR = 4;

/**
 * The mean time to the next comparison at a random time, in milliseconds, for a page whose
 * comparison costs `compareCostMs`, while the latest comparison was clean (`clean`) or not.
 *
 * While the host may show the frame a sample is two comparisons (`sampleAroundPaint`), so the mean
 * is 2 * SAMPLE_COST_FACTOR times the cost and the samples take 1 / SAMPLE_COST_FACTOR of the
 * thread, for every page up to the cost SAMPLE_MEAN_MAX_MS allows. At most one comparison runs per
 * sample otherwise (`sample`), so the sparser mean while something was found takes at most half
 * that share. Pages of 25 ms or less keep the mean they had before SAMPLE_MEAN_MAX_MS was raised
 * from 1 s (Laysi/trustiot-v2#1081).
 */
export function sampleMeanMs(compareCostMs: number, clean: boolean): number {
  const mean = Math.min(SAMPLE_MEAN_MAX_MS, Math.max(SAMPLE_MEAN_MIN_MS, SAMPLE_COST_FACTOR * 2 * compareCostMs));
  return clean ? mean : Math.min(SAMPLE_MEAN_MAX_MS, mean * SAMPLE_IDLE_FACTOR);
}

/**
 * The moving average of what a comparison of the page costs, after one that took `tookMs`
 * (`averageMs` is 0 before the first).
 */
export function averageCompareCost(averageMs: number, tookMs: number): number {
  return averageMs === 0 ? tookMs : 0.8 * averageMs + 0.2 * tookMs;
}

/** How far above the latest two comparisons the cost the samples are drawn from may stay (`sampleCostMs`). */
export const SAMPLE_RECENT_FACTOR = 2;

/**
 * What the next samples take a comparison to cost (`sampleMeanMs`): the moving average, but at most
 * SAMPLE_RECENT_FACTOR times the costlier of the latest two comparisons (`latestMs`, `previousMs`).
 * On a page that stays the same the bound does not bind, so the samples keep 1 / SAMPLE_COST_FACTOR
 * of the thread; after one costly comparison the next two bring the cost back to at most twice the
 * page's (Laysi/trustiot-v2#1081). The average alone took many comparisons to come down. A plain
 * minimum of the average and the latest comparison is biased low, as it always picks the cheaper
 * (the comparison after a paint costs less than the one before it, and the noise is skewed): it
 * sampled static pages of 1000 and 3000 marks at 7.0% and 6.8% of the thread (2026-11-05
 * measurement).
 */
export function sampleCostMs(averageMs: number, latestMs: number, previousMs: number): number {
  return Math.min(averageMs, SAMPLE_RECENT_FACTOR * Math.max(latestMs, previousMs));
}

/** A beat this much later than due means the thread was blocked: compare at once. */
export const LATE_BEAT_MS = 250;

/**
 * Whether a heartbeat that runs at `nowMs`, due at `dueMs`, says the thread was blocked: it is late
 * by more than LATE_BEAT_MS besides `ownMs`, the runtime's own comparisons that ran after it was due
 * (`overdueMs`). Those are not a blocked thread: counting them, the two back-to-back comparisons of
 * one sample on a large page (178 ms each) made the next beat late, the comparisons that beat ran
 * made the one after it late, and so on, so the runtime compared without pause (97% of the thread,
 * 2026-11-05, Laysi/trustiot-v2#1081). The page's code never runs during a comparison, so the time
 * the page blocks the thread still counts in full.
 */
export function beatIsLate(nowMs: number, dueMs: number, ownMs: number): boolean {
  return nowMs - dueMs - ownMs > LATE_BEAT_MS;
}

/** The part of a comparison from `startedMs` to `endedMs` after the heartbeat due at `dueMs`. */
export function overdueMs(startedMs: number, endedMs: number, dueMs: number): number {
  return Math.max(0, endedMs - Math.max(startedMs, dueMs));
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
 * the runtime hides the page within its own document, which the page's scripts can reach, and
 * reports `failed` on its port, so the host removes the frame (README, 「顯示時比對的強制」): the
 * runtime does not keep correcting a page that fights it. Rechecks and data refreshes count alike,
 * so a page that draws a value it cannot back on every refresh reaches it too; only a comparison
 * with nothing to rewrite starts the count again, so a page that keeps updating with correct values
 * never reaches it.
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
  const microtask = win.queueMicrotask;
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
  const getDescriptor = Object.getOwnPropertyDescriptor;
  const log = Math.log;
  // The randomness of the sampling: the browser's CSPRNG, captured before the page runs. Never
  // Math.random, whose state the page's own calls reveal.
  const cryptoObject = (win as unknown as { crypto?: Crypto }).crypto;
  const getRandomValues = cryptoObject?.getRandomValues as Crypto["getRandomValues"] | undefined;
  const visibilityState = (() => {
    for (let prototype: object | null = win.Document.prototype; prototype !== null; ) {
      const get = getDescriptor(prototype, "visibilityState")?.get;
      if (get !== undefined) return get;
      prototype = Object.getPrototypeOf(prototype) as object | null;
    }
    return undefined;
  })();
  const pixelRatio = (() => {
    for (let target: object | null = win; target !== null; ) {
      const get = getDescriptor(target, "devicePixelRatio")?.get;
      if (get !== undefined) return get;
      target = Object.getPrototypeOf(target) as object | null;
    }
    return undefined;
  })();
  const matchMedia = (win as unknown as { matchMedia?: (query: string) => MediaQueryList }).matchMedia;
  // A channel of the runtime's own, made before the page runs, for a task right after a paint
  // (`afterPaint`): the page has no reference to it.
  const MessageChannelConstructor = (win as unknown as { MessageChannel?: typeof MessageChannel }).MessageChannel;
  const portPostMessage = (win as unknown as { MessagePort?: typeof MessagePort }).MessagePort?.prototype.postMessage;
  const portStart = (win as unknown as { MessagePort?: typeof MessagePort }).MessagePort?.prototype.start;
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
  /** The port to the host in display mode, from `init`; `null` in check mode. */
  let report: Reporter | null = null;
  /**
   * Check mode reports through the window (`env.post`). In display mode errors, sizes and sources
   * go on the port, where the page cannot forge them; `returned` and `rendered` still go through
   * the window, where the host does not listen any more (they say less than `compared`).
   */
  const post = (message: SandboxMessage) => {
    if (report !== null && (message.type === "error" || message.type === "resize" || message.type === "source")) {
      report(message);
      return;
    }
    env.post(message);
  };

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
  /**
   * Display mode: watches the whole document from before the first page script, so every change
   * the page makes is compared again before the browser paints it (`requestCompare`).
   */
  let guard: MutationObserver | null = null;
  /** A comparison is queued for the end of the current task (`requestCompare`). */
  let compareQueued = false;
  /** A comparison is waiting for the next animation frame (`poke`). */
  let poked = false;
  /** The last heartbeat's number. */
  let beats = 0;
  /** When the next heartbeat is due (`now()`), to tell a blocked thread by a late one. */
  let beatDue = 0;
  /** How long the runtime's own comparisons ran after the next heartbeat was due (`beatIsLate`). */
  let comparedOverdue = 0;
  /** The latest display-time comparison found nothing. */
  let lastClean = false;
  /**
   * The runtime was told of a change since the latest comparison (a DOM mutation, a change through
   * an interface `seal.ts` wraps, a poke, new data or a render): a finding then has a cause it knows.
   */
  let notified = true;
  /** The next comparison at a random time (`sample`). */
  let sampleTimer: number | undefined;
  /** What a comparison of this page costs, in milliseconds (`averageCompareCost`). */
  let compareCost = 0;
  /** What the latest comparison and the one before it cost, in milliseconds (`sampleCostMs`). */
  let latestCost = 0;
  let previousCost = 0;
  const randomWords = new Uint32Array(64);
  let randomIndex = randomWords.length;
  /** The document is hidden in itself until a comparison finds nothing (`data-tq-unverified`). */
  let unverified = false;
  /** The latest `compared` the host got was clean: it may be showing the frame. */
  let reportedClean = false;
  /** Counts findings and new data: a clean result's wait for a painted frame (`reportCompared`) ends on either. */
  let showWait = 0;
  /** A clean result is waiting for a painted frame (`reportCompared`). */
  let showWaiting = false;
  /** A render is running or settling: its own comparison corrects when it is done (`settle`). */
  let drawing = false;
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
    // Not a notice by itself: what a render changes is a DOM mutation or a sealed interface, which
    // notify, and new data notifies where it arrives (`deliver`). A notice here would last until
    // the next comparison, so a page re-registering a render that draws nothing (`tq.on`) could
    // keep a change it is not reporting explained.
    const mine = ++attempt;
    stopSettling();
    // This render's own comparison corrects when it settles; an earlier pending one is moot.
    drawing = true;
    cancel(recheck);
    recheck = undefined;
    drawnWith = render;
    const started = now();
    let returned: unknown;
    try {
      returned = apply(render, undefined, [pageApi]);
    } catch (error) {
      drawing = false;
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
      drawing = false;
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
   * the guard (`requestCompare`) takes over from there. Only the latest render waits: a newer one stops this
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
          drawing = false;
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

  /** Drops the guard's pending records: the runtime's own changes are not a reason to compare. */
  const takeGuardRecords = () => {
    if (guard !== null) apply(takeRecords, guard, []);
  };

  /**
   * Hides the document in itself while the latest comparison found something (or a newer one is
   * pending), so a browser that runs the frame in the host page's process, where the host learns
   * of it only after the frame painted, paints nothing rather than the value. The host hides the
   * frame on the same report; this is page-reachable state and only narrows that window.
   */
  const markUnverified = () => {
    if (unverified) return;
    unverified = true;
    const root = dom.documentElement();
    if (root !== null) dom.setAttribute(root, "data-tq-unverified", "");
  };

  const clearUnverified = () => {
    if (!unverified) return;
    unverified = false;
    const root = dom.documentElement();
    if (root !== null && dom.getAttribute(root, "data-tq-unverified") !== null) {
      dom.removeAttribute(root, "data-tq-unverified");
    }
  };

  /**
   * Reports a comparison's result. A finding is reported at once. A clean result the host would
   * show the frame on (the first one, or one after a finding) waits until the frame has painted a
   * compared state: two animation frames with no finding (every change the runtime is told of is
   * compared before the frame paints it, so the frame's latest paint was of a clean state), then
   * the document is compared once more (`painted`) and that result reported. Without the wait the
   * host would show the frame while the browser still displays an earlier paint, which may be the
   * changed state the finding was about: the browser paints a frame the host hides only about once
   * a second (README, 「顯示時比對的強制」), and a page can undo the runtime's own hiding
   * (`data-tq-unverified`). A finding or new data ends the wait; while a render settles, its own
   * comparison reports (`settle`); a frame that does not render (a hidden tab) stays hidden until
   * it does.
   */
  const reportCompared = (findings: number, painted = false) => {
    if (report === null || delivered === null) return;
    if (findings > 0 || reportedClean || painted) {
      const shown = findings === 0 && !reportedClean;
      reportedClean = findings === 0;
      showWait++;
      showWaiting = false;
      report({ tq: PROTOCOL, type: "compared", generation, findings });
      // The host may show the frame from now on: the next sample, drawn while it was hidden with the
      // sparser mean (SAMPLE_IDLE_FACTOR), is drawn again with the shown frame's. The intervals are
      // memoryless, so drawing again tells the page nothing.
      if (shown) scheduleSample();
      return;
    }
    if (drawing || showWaiting) return;
    showWaiting = true;
    const mine = showWait;
    frame(() =>
      frame(() => {
        if (mine !== showWait) return;
        showWaiting = false;
        if (comparisonFailed || report === null || drawing) return;
        readOnlyCompare(false, true);
      }),
    );
  };

  /** Hides the whole page for good, within its own document, and tells the host (display mode). */
  const hideForGood = (reason: "comparison" | "corrections" | "unnotified") => {
    if (comparisonFailed) return;
    comparisonFailed = true;
    cancel(recheck);
    cancel(sampleTimer);
    if (guard !== null) apply(disconnect, guard, []);
    const root = dom.documentElement();
    if (root !== null) dom.setAttribute(root, "data-tq-flag-hide", "");
    if (report !== null) report({ tq: PROTOCOL, type: "failed", reason });
  };

  /**
   * The display-time comparison that corrects. It runs in the page's realm; the realm's built-ins
   * are frozen (harden.ts) and the DOM functions were captured before the page ran (dom.ts), so
   * replacing them does not change what it computes from a given DOM. What it does with the result
   * (corrected text, flags, the hidden document) lives in the page's own document and is not a
   * boundary: the host enforces the result it reports on the port. If it throws, the document is
   * hidden for good (`hideForGood`).
   */
  const showChecked = () => {
    if (comparisonFailed) return;
    try {
      compareAndShow();
    } catch {
      hideForGood("comparison");
    }
  };

  /**
   * Schedules the next correcting comparison after a comparison found something: normally once the
   * DOM is quiet (QUIET_PERIOD_MS after the last change), but never later than MAX_RECHECK_DELAY_MS
   * after the first change of the burst, so a page that keeps mutating cannot postpone it.
   */
  const scheduleRecheck = () => {
    if (comparisonFailed) return;
    const nowMs = now();
    if (recheck === undefined) recheckBy = nowMs + MAX_RECHECK_DELAY_MS;
    cancel(recheck);
    recheck = later(
      () => {
        recheck = undefined;
        showChecked();
      },
      Math.max(0, Math.min(QUIET_PERIOD_MS, recheckBy - nowMs)),
    );
  };

  const compareAndShow = () => {
    if (delivered === null || init === null) return;
    const model = delivered.model;
    const locale = init.locale;
    // Flags describe this comparison only: a value a later render corrected is shown plainly
    // again. Text the runtime replaced stays replaced until the page writes it again.
    clearUnverified();
    for (const element of flagged) {
      for (const name of FLAGS) if (dom.getAttribute(element, name) !== null) dom.removeAttribute(element, name);
    }
    flagged = [];
    // Everything the page changed so far is in the DOM compared now.
    takeGuardRecords();
    const { findings } = timed(() => compareDocument(dom, model, locale, env.layout));
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
        hideForGood("corrections");
        return;
      }
    } else {
      correctionStreak = 0;
    }
    if (findings.length > 0) markUnverified();
    // The runtime's own changes above are not a reason to compare again.
    takeGuardRecords();
    settled(findings.length);
    reportCompared(findings.length);
  };

  /**
   * Runs a comparison and keeps what comparisons of this page cost (`sampleMean`), and how much of
   * it delayed the next heartbeat (`beatIsLate`).
   */
  const timed = <T>(compare: () => T): T => {
    const started = now();
    const result = compare();
    const ended = now();
    const took = ended - started;
    compareCost = averageCompareCost(compareCost, took);
    previousCost = latestCost;
    latestCost = took;
    comparedOverdue += overdueMs(started, ended, beatDue);
    return result;
  };

  /** A comparison found `findings`: the state the next one starts from. */
  const settled = (findings: number) => {
    lastClean = findings === 0;
    notified = false;
  };

  /**
   * The display-time comparison that only reads, after every change the runtime is told of, at the
   * end of the task that made it (before the browser paints); `sample` runs the same one at random
   * times. It reports the result on the port; when it found something, the document is hidden in
   * itself and the correcting comparison follows (`scheduleRecheck`).
   */
  const quickCompare = () => {
    compareQueued = false;
    if (comparisonFailed || report === null || delivered === null || init === null) return;
    readOnlyCompare(false);
  };

  /**
   * Compares the document as it is and reports the result, changing nothing in it but the
   * `data-tq-unverified` state. `random` is a comparison at a random time (`sample`): if it finds
   * something while the latest comparison was clean and the runtime was told of no change since,
   * the page changed what it shows by a way the runtime is not told of (a browser interface
   * `seal.ts` does not wrap), and the runtime gives up on the page for good. `painted` is the
   * comparison after a clean result's wait for a painted frame (`reportCompared`).
   */
  const readOnlyCompare = (random: boolean, painted = false) => {
    if (delivered === null || init === null) return;
    const model = delivered.model;
    const locale = init.locale;
    clearUnverified();
    // Records still pending are changes the runtime has not compared yet: a cause it knows.
    const pending = guard !== null && (apply(takeRecords, guard, []) as unknown[]).length > 0;
    const explained = notified || pending || !lastClean;
    let findings: number;
    try {
      findings = timed(() => compareDocument(dom, model, locale, env.layout).findings.length);
    } catch {
      hideForGood("comparison");
      return;
    }
    if (random && findings > 0 && !explained) {
      hideForGood("unnotified");
      return;
    }
    if (findings > 0) {
      markUnverified();
      // A render still settling corrects when it is done (`settle`); otherwise correct now.
      if (!drawing) scheduleRecheck();
    }
    takeGuardRecords();
    settled(findings);
    reportCompared(findings, painted);
  };

  /** Compares at the end of the current task: after the page's script, before the next paint. */
  const requestCompare = () => {
    notified = true;
    if (compareQueued || comparisonFailed || report === null) return;
    compareQueued = true;
    apply(microtask, win, [quickCompare]);
  };

  /**
   * A user action or a viewport change may restyle the page without changing its DOM (`:hover`,
   * `:focus`, a media query): compare at the end of the task that delivered it, as after a DOM
   * mutation, and again in the next animation frame, which runs before that frame's style, layout
   * and paint. The notice ends with the task: what the page changes in a later task is not
   * explained by it (`readOnlyCompare`), so an event the page causes itself (`el.focus()`,
   * `scrollBy`) cannot cover a change the runtime is not told of.
   */
  const poke = () => {
    requestCompare();
    if (poked || comparisonFailed || report === null) return;
    poked = true;
    frame(() => {
      poked = false;
      requestCompare();
    });
  };

  /**
   * Only events the browser dispatched: a page's own `dispatchEvent` notifies nothing. A trusted
   * event the page causes (`el.focus()`, `scrollBy()`) still notifies, and that is safe: the notice
   * lasts only until the comparison at the end of its task (`requestCompare`; `settled` clears it),
   * and no page code runs between the two but the rest of that task. What that task changed is in
   * that comparison, which reports it as a finding (the host hides the frame and counts a
   * regression); a change in any later task is unexplained to a random comparison. So such an event
   * can turn a close for good into a hidden frame that is closed after MAX_REGRESSIONS, never into
   * a value left on screen.
   */
  const trusted =
    (listener: () => void) =>
    (event: Event): void => {
      if (env.isTrusted(event)) listener();
    };

  /**
   * The heartbeat: says the runtime's thread runs, and compares nothing (its fixed period would tell
   * the page when). A late one means the thread was blocked, by the page or by the device: compare
   * at once, so the host, which hid the frame meanwhile (`STALE_AFTER_MS`), can show it again. The
   * runtime's own comparisons do not make a beat late (`beatIsLate`): what they found is reported as
   * after any comparison, so comparing again at once would only repeat them.
   */
  const beat = () => {
    if (report === null || comparisonFailed) return;
    const late = beats > 0 && beatIsLate(now(), beatDue, comparedOverdue);
    comparedOverdue = 0;
    report({ tq: PROTOCOL, type: "beat", seq: ++beats });
    beatDue = now() + HEARTBEAT_INTERVAL_MS;
    later(beat, HEARTBEAT_INTERVAL_MS);
    if (late) sampleNow();
  };

  /** A uniform random number in (0, 1) from the captured CSPRNG. */
  const uniform = (): number => {
    if (randomIndex >= randomWords.length) {
      apply(getRandomValues as Crypto["getRandomValues"], cryptoObject, [randomWords]);
      randomIndex = 0;
    }
    return ((randomWords[randomIndex++] as number) + 0.5) / 4294967296;
  };

  /** The mean time to the next comparison at a random time (`sampleMeanMs`). */
  const sampleMean = () => sampleMeanMs(sampleCostMs(compareCost, latestCost, previousCost), lastClean);

  const scheduleSample = () => {
    cancel(sampleTimer);
    sampleTimer = undefined;
    if (comparisonFailed || report === null) return;
    sampleTimer = later(sample, -sampleMean() * apply(log, Math, [uniform()]));
  };

  /**
   * Runs `callback` in a task soon after the next paint, posted from an animation frame callback on
   * the runtime's own channel. Best effort: the order of tasks is not guaranteed (README,
   * 「還擋不住的」).
   */
  let afterPaintQueue: (() => void)[] = [];
  let tick: MessageChannel | null = null;
  const afterPaint = (callback: () => void) => {
    if (tick === null && MessageChannelConstructor !== undefined && portPostMessage !== undefined) {
      try {
        const channel = new MessageChannelConstructor();
        apply(addEventListener, channel.port1, [
          "message",
          () => {
            const queued = afterPaintQueue;
            afterPaintQueue = [];
            for (const run of queued) run();
          },
        ]);
        if (portStart !== undefined) apply(portStart, channel.port1, []);
        tick = channel;
      } catch {
        tick = null;
      }
    }
    if (tick === null || portPostMessage === undefined) {
      later(callback, 0);
      return;
    }
    afterPaintQueue.push(callback);
    apply(portPostMessage, tick.port2, [0]);
  };

  /** A comparison around the next paint is pending (`sampleAroundPaint`). */
  let paintSamplePending = false;

  /**
   * Compares just before the next paint (in an animation frame callback) and soon after it
   * (`afterPaint`), so a value the page shows only around paints is more likely in one of the two
   * than in a comparison at any time. It bounds nothing: a page with a path the runtime is not told
   * of can time a value so that no sample sees it; the defence is a complete seal (README,
   * 「還擋不住的」).
   */
  const sampleAroundPaint = () => {
    if (paintSamplePending) return;
    paintSamplePending = true;
    frame(() => {
      paintSamplePending = false;
      if (comparisonFailed || report === null) return;
      readOnlyCompare(true);
      // Only while the host may show the frame: a task posted from an animation frame of a hidden
      // frame makes Chromium throttle its animation frames at once, which delays showing it again.
      if (comparisonFailed || !reportedClean) return;
      afterPaint(() => {
        if (!comparisonFailed && report !== null) readOnlyCompare(true);
      });
    });
  };

  /**
   * A sample: after a random time, comparisons around the next paint while the host may show the
   * frame (`reportedClean`), or one comparison now while it does not; see `readOnlyCompare`.
   */
  const sample = () => {
    sampleTimer = undefined;
    if (comparisonFailed || report === null) return;
    if (reportedClean) sampleAroundPaint();
    else readOnlyCompare(true);
    scheduleSample();
  };

  /** A comparison now (at a time the page did not choose), and a sample. */
  const sampleNow = () => {
    cancel(sampleTimer);
    sampleTimer = undefined;
    if (comparisonFailed || report === null) return;
    readOnlyCompare(true);
    if (comparisonFailed) return;
    sampleAroundPaint();
    scheduleSample();
  };

  /**
   * Changes the page cannot see coming from its own DOM: the user's media preferences and screen,
   * fonts that finish loading, a same-document navigation (`:target`), and the document coming back
   * into view (compared at once, at a time the page did not choose: `sample`).
   */
  const watchEnvironment = () => {
    const listen = (target: unknown, type: string, listener: () => void, options?: unknown) => {
      if (typeof target !== "object" || target === null) return;
      try {
        apply(addEventListener, target, [type, trusted(listener), options]);
      } catch {
        // Not an event target in this DOM.
      }
    };
    const media = (query: string): MediaQueryList | null => {
      if (typeof matchMedia !== "function") return null;
      try {
        return apply(matchMedia, win, [query]) as MediaQueryList;
      } catch {
        return null;
      }
    };
    for (const query of MEDIA_FEATURES) listen(media(query), "change", poke);
    const watchResolution = () => {
      const ratio = pixelRatio === undefined ? undefined : apply(pixelRatio, win, []);
      if (typeof ratio !== "number" || !Number.isFinite(ratio)) return;
      listen(
        media(`(resolution: ${ratio}dppx)`),
        "change",
        () => {
          poke();
          watchResolution();
        },
        { once: true },
      );
    };
    watchResolution();
    const fonts = (doc as unknown as { fonts?: unknown }).fonts;
    for (const type of ["loadingdone", "loadingerror"]) listen(fonts, type, poke);
    const navigation = (win as unknown as { navigation?: unknown }).navigation;
    for (const type of ["navigate", "currententrychange"]) listen(navigation, type, requestCompare);
    listen(doc, "visibilitychange", () => {
      if (visibilityState !== undefined && apply(visibilityState, doc, []) === "visible") sampleNow();
    });
  };

  // --- the host's messages -------------------------------------------------------------------

  const reportHeight = () => {
    const root = dom.documentElement();
    // A document hidden in itself has no height to show; the host keeps the frame's size.
    if (root === null || unverified) return;
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
      // Watch the document before the first page script runs, so no change goes uncompared.
      guard = new MutationObserverConstructor(() => requestCompare());
      apply(observe, guard, [doc, { subtree: true, childList: true, attributes: true, characterData: true }]);
      for (const type of POKES) apply(addEventListener, win, [type, trusted(poke), true]);
      for (const type of NOTICES) apply(addEventListener, win, [type, trusted(requestCompare), true]);
      // A style sheet or anything else that loads applies when it has loaded, without a DOM change:
      // element load events reach the document but not the window.
      for (const type of ["load", "error"]) apply(addEventListener, doc, [type, trusted(poke), true]);
      watchEnvironment();
      // Without the CSPRNG the comparisons at random times would be predictable: show nothing.
      if (typeof getRandomValues !== "function") {
        hideForGood("comparison");
        return;
      }
      beatDue = now();
      beat();
      scheduleSample();
      apply(addEventListener, doc, [
        "click",
        (event: Event) => {
          // A page can dispatch a click of its own; only the user's opens the source panel.
          if (!env.isTrusted(event)) return;
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
    // Not from inside the `init` event: while it is dispatched, `window.event` is that event, and
    // a page script run now could read the port from its `ports`.
    later(() => loadPage(message, tq), 0);
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
    // The host hides the frame until this data is compared clean (guard.ts, `refresh`).
    reportedClean = false;
    showWait++;
    showWaiting = false;
    // The document now shows the earlier data's values until the render redraws it: findings until
    // then have a cause the runtime knows.
    notified = true;
    startRender(generation);
  };

  const receive = (data: unknown, port?: Reporter) => {
    if (init === null) {
      const parsed = InitMessageSchema.safeParse(data);
      if (!parsed.success) return;
      // Display mode needs the port: without it the host could only learn the result from messages
      // the page can forge, so the runtime does not start.
      if (parsed.data.mode === "view") {
        if (port === undefined) return;
        report = port;
      }
      start(parsed.data);
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

  env.post({ tq: PROTOCOL, type: "ready", runtime: 1 });
  return { receive, styleChanged: requestCompare };
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
  const ports = Object.getOwnPropertyDescriptor(win.MessageEvent.prototype, "ports")?.get;
  const portPost = (win as unknown as { MessagePort?: typeof MessagePort }).MessagePort?.prototype.postMessage;
  const stopImmediately = win.Event.prototype.stopImmediatePropagation;
  if (source === undefined || data === undefined || ports === undefined || portPost === undefined) return null;
  // Freeze the realm's built-ins before the first page script runs (on `init`, which arrives after
  // this returns): the display-time comparison and the formatters it calls then read genuine
  // intrinsics whatever the page does. The checker compares from a separate isolated world and does
  // not rely on this; it hardens here too so a page runs under the same realm in both modes.
  harden();
  let runtime: Runtime | null = null;
  // Before any page script as well: the CSS object model reports its changes, and the ways to build
  // content the comparison cannot read are closed (seal.ts).
  seal(win, { styleChanged: () => runtime?.styleChanged() });
  const started = startRuntime({
    window: win,
    post: (message) => apply(postMessage, parent, [message, "*"]),
    d3: (win as unknown as { d3?: unknown }).d3,
    // The sandbox frame is a browser document, which is laid out.
    layout: true,
    isTrusted,
  });
  runtime = started;
  apply(win.EventTarget.prototype.addEventListener, win, [
    "message",
    (event: MessageEvent) => {
      if (!isTrusted(event) || apply(source, event, []) !== parent) return;
      // The port, if the host sent one: kept in this closure, posted to only with the
      // postMessage captured above, so the page never gets hold of it. The host sends it once,
      // with `init`, and the page's markup and scripts arrive in that same message, so no page
      // listener exists while that event is dispatched; the page loads in a later task
      // (`loadPage`), when `window.event` no longer refers to it. As a second line, an event that
      // carries a port goes no further than this listener, which the runtime registered before
      // any page script, and only the first `init` is taken (`receive`).
      const transferred = apply(ports, event, []) as readonly MessagePort[];
      if (transferred.length > 0) apply(stopImmediately, event, []);
      const port = transferred.length === 1 ? transferred[0] : undefined;
      started.receive(
        apply(data, event, []),
        port === undefined ? undefined : (message) => apply(portPost, port, [message]),
      );
    },
  ]);
  return started;
}
