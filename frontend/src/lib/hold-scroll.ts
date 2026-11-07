/** How long the page's height must stay unchanged before a restored position is let go. */
export const HOLD_QUIET_MS = 1000;

/** The longest a restored position is held, whatever the page does. */
export const HOLD_BOUND_MS = 10_000;

/** What `holdScroll` needs from the browser; the tests pass fakes. */
export interface HoldScrollEnvironment {
  /** The window's current vertical scroll position. */
  scrollY(): number;
  scrollTo(top: number): void;
  /** Calls `callback` whenever the page's height may have changed; returns the unsubscribe. */
  onResize(callback: () => void): () => void;
  /** Calls `callback` at the reader's next wheel, touch, key or pointer press; returns the unsubscribe. */
  onUserInput(callback: () => void): () => void;
  /** Calls `callback` after `ms`; returns the cancel. */
  later(callback: () => void, ms: number): () => void;
}

/**
 * Keeps the page at `top` while it grows. The browser's back button restores a position once, at
 * the moment the page is shown; a page whose content arrives afterwards (charts, queries that run
 * again) is still too short then, so the position is clamped, and growth above the reader's place
 * moves what they were looking at. This puts the position back at every height change.
 *
 * It lets go at the reader's first input (they now decide where the page is), after the page's
 * height has not changed for `HOLD_QUIET_MS` (everything has arrived), or after `HOLD_BOUND_MS`.
 * `onDone` is called once, whichever way it ends. Returns the function that stops it earlier
 * (leaving the page).
 */
export function holdScroll(top: number, env: HoldScrollEnvironment, onDone?: () => void): () => void {
  let stopped = false;
  const cleanups: (() => void)[] = [];
  let cancelQuiet: (() => void) | null = null;

  const stop = () => {
    if (stopped) return;
    stopped = true;
    cancelQuiet?.();
    for (const cleanup of cleanups) cleanup();
    onDone?.();
  };
  const restore = () => {
    if (Math.abs(env.scrollY() - top) > 1) env.scrollTo(top);
  };
  const armQuiet = () => {
    cancelQuiet?.();
    cancelQuiet = env.later(stop, HOLD_QUIET_MS);
  };

  cleanups.push(env.onUserInput(stop));
  cleanups.push(
    env.onResize(() => {
      if (stopped) return;
      restore();
      armQuiet();
    }),
  );
  cleanups.push(env.later(stop, HOLD_BOUND_MS));
  restore();
  armQuiet();
  return stop;
}

/** `holdScroll`'s environment for a browser window. */
export function windowScrollEnvironment(win: Window = window): HoldScrollEnvironment {
  const inputs = ["wheel", "touchstart", "keydown", "pointerdown"] as const;
  return {
    scrollY: () => win.scrollY,
    scrollTo: (top) => win.scrollTo({ top, left: 0, behavior: "instant" }),
    onResize: (callback) => {
      if (typeof ResizeObserver !== "function") return () => {};
      const observer = new ResizeObserver(callback);
      observer.observe(win.document.documentElement);
      return () => observer.disconnect();
    },
    onUserInput: (callback) => {
      for (const name of inputs) win.addEventListener(name, callback, { passive: true, capture: true });
      return () => {
        for (const name of inputs) win.removeEventListener(name, callback, { capture: true });
      };
    },
    later: (callback, ms) => {
      const id = win.setTimeout(callback, ms);
      return () => win.clearTimeout(id);
    },
  };
}
