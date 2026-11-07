import { describe, expect, test } from "bun:test";

import { HOLD_BOUND_MS, HOLD_QUIET_MS, type HoldScrollEnvironment, holdScroll } from "./hold-scroll.ts";

/**
 * A page that clamps `scrollTo` to its height and a clock the test advances by hand, so nothing
 * here waits for real time.
 */
function fakePage(initialMax: number, initialY = 0) {
  let max = initialMax;
  let y = initialY;
  let now = 0;
  const resizeListeners = new Set<() => void>();
  const inputListeners = new Set<() => void>();
  const timers = new Map<number, { at: number; callback: () => void }>();
  let nextTimer = 0;
  const calls: number[] = [];
  const env: HoldScrollEnvironment = {
    scrollY: () => y,
    scrollTo: (top) => {
      calls.push(top);
      y = Math.min(top, max);
    },
    onResize: (callback) => {
      resizeListeners.add(callback);
      return () => resizeListeners.delete(callback);
    },
    onUserInput: (callback) => {
      inputListeners.add(callback);
      return () => inputListeners.delete(callback);
    },
    later: (callback, ms) => {
      const id = nextTimer++;
      timers.set(id, { at: now + ms, callback });
      return () => timers.delete(id);
    },
  };
  return {
    env,
    calls,
    y: () => y,
    listeners: () => resizeListeners.size + inputListeners.size,
    pending: () => timers.size,
    /** The page grows (or shrinks) to `newMax`; the browser's own move is `shift`. */
    resize(newMax: number, shift = 0) {
      max = newMax;
      y = Math.min(y + shift, max);
      for (const listener of [...resizeListeners]) listener();
    },
    /** The reader scrolls to `to` with the wheel. */
    wheel(to: number) {
      for (const listener of [...inputListeners]) listener();
      y = Math.min(to, max);
    },
    advance(ms: number) {
      now += ms;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= now) {
          timers.delete(id);
          timer.callback();
        }
      }
    },
  };
}

describe("holdScroll", () => {
  test("puts the position back as the page grows, also when the browser moved it", () => {
    const page = fakePage(500);
    holdScroll(789, page.env);
    expect(page.y()).toBe(500); // clamped: the page is still short
    page.resize(700);
    expect(page.y()).toBe(700);
    page.resize(900, 70); // the browser's scroll anchoring moves it as content above grows
    expect(page.y()).toBe(789);
  });

  test("does not scroll when the position is already right", () => {
    const page = fakePage(1000, 400);
    holdScroll(400, page.env);
    page.resize(1100);
    expect(page.calls).toEqual([]);
  });

  test("lets go at the reader's first input, and leaves where they put the page", () => {
    const page = fakePage(500);
    holdScroll(789, page.env);
    page.wheel(100);
    page.resize(900);
    expect(page.y()).toBe(100);
    expect(page.listeners()).toBe(0);
    expect(page.pending()).toBe(0);
  });

  test("lets go once the height has not changed for the quiet time, and a change restarts it", () => {
    const page = fakePage(500);
    holdScroll(789, page.env);
    page.advance(HOLD_QUIET_MS - 1);
    page.resize(600); // restarts the quiet time
    page.advance(HOLD_QUIET_MS - 1);
    expect(page.listeners()).toBeGreaterThan(0);
    page.advance(1);
    expect(page.listeners()).toBe(0);
    page.resize(900);
    expect(page.y()).toBe(600);
  });

  test("lets go at the bound however often the page changes", () => {
    const page = fakePage(500);
    holdScroll(789, page.env);
    for (let elapsed = 0; elapsed < HOLD_BOUND_MS; elapsed += HOLD_QUIET_MS / 2) {
      page.resize(500);
      page.advance(HOLD_QUIET_MS / 2);
    }
    expect(page.listeners()).toBe(0);
    expect(page.pending()).toBe(0);
  });

  test("says once that it is done, whichever way it ends", () => {
    type Page = ReturnType<typeof fakePage>;
    const ends: ((page: Page, stop: () => void) => void)[] = [
      (page) => page.wheel(10),
      (page) => page.advance(HOLD_QUIET_MS),
      (_page, stop) => stop(),
    ];
    for (const end of ends) {
      const page = fakePage(500);
      let done = 0;
      const stop = holdScroll(789, page.env, () => {
        done += 1;
      });
      end(page, stop);
      stop();
      expect(done).toBe(1);
    }
  });

  test("the returned function stops it", () => {
    const page = fakePage(500);
    const stop = holdScroll(789, page.env);
    stop();
    expect(page.listeners()).toBe(0);
    expect(page.pending()).toBe(0);
    page.resize(900);
    expect(page.y()).toBe(500);
  });
});
