import { setSystemTime } from "bun:test";

import { AUTO_REFRESH_MS, type AutoRefreshClock } from "../pages/device/autoRefresh.ts";

/**
 * The device page's refresh timer under the test's control: a callback runs when `advance` moves
 * the clock past it, never by itself, so no test waits a real minute and a test that expects no
 * refresh does not depend on how long it waits. `advance` also moves `Date.now()` (Bun's
 * `setSystemTime`), which the page reads for the end of its range; `restore` puts the real clock
 * back and belongs in `afterEach`.
 */
export interface ManualClock extends AutoRefreshClock {
  /** The callbacks waiting for their time. */
  pending(): number;
  /** Moves the clock forward by `ms` and runs what falls due, in order. */
  advance(ms: number): void;
  restore(): void;
}

export function manualClock(start: Date = new Date()): ManualClock {
  let now = start.getTime();
  let nextId = 0;
  const timers = new Map<number, { due: number; run: () => void }>();
  setSystemTime(new Date(now));
  return {
    intervalMs: AUTO_REFRESH_MS,
    after: (delayMs, run) => {
      const id = nextId;
      nextId += 1;
      timers.set(id, { due: now + delayMs, run });
      return () => {
        timers.delete(id);
      };
    },
    pending: () => timers.size,
    advance: (ms) => {
      const target = now + ms;
      for (;;) {
        const due = [...timers.entries()]
          .filter(([, timer]) => timer.due <= target)
          .sort((a, b) => a[1].due - b[1].due)[0];
        if (due === undefined) break;
        timers.delete(due[0]);
        now = Math.max(now, due[1].due);
        setSystemTime(new Date(now));
        due[1].run();
      }
      now = target;
      setSystemTime(new Date(now));
    },
    restore: () => {
      timers.clear();
      setSystemTime();
    },
  };
}
