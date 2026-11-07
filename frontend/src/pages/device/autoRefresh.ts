import { createContext, useContext, useEffect, useRef, useSyncExternalStore } from "react";

/**
 * How often an open device page asks again: the gateway samples every 60 seconds (the maintainer's
 * decision of 2026-11-02), so a shorter interval would mostly fetch what the page already has and a
 * longer one would show a reading a minute late.
 */
export const AUTO_REFRESH_MS = 60_000;

/**
 * The page's timer, as a seam the tests replace (src/test/manual-clock.ts): no test waits a real
 * minute. Time itself is `Date.now()`; the clock only decides when a callback runs.
 */
export interface AutoRefreshClock {
  intervalMs: number;
  /** Runs `run` once after `delayMs`; the returned function cancels it. */
  after(delayMs: number, run: () => void): () => void;
}

const realClock: AutoRefreshClock = {
  intervalMs: AUTO_REFRESH_MS,
  after: (delayMs, run) => {
    const timer = setTimeout(run, delayMs);
    return () => clearTimeout(timer);
  },
};

export const AutoRefreshClockContext = createContext<AutoRefreshClock>(realClock);

function subscribeToVisibility(onChange: () => void): () => void {
  document.addEventListener("visibilitychange", onChange);
  return () => document.removeEventListener("visibilitychange", onChange);
}

/** Whether the page is in the foreground (`document.visibilityState`); a hidden tab has no one to show new readings to. */
export function usePageVisible(): boolean {
  return useSyncExternalStore(
    subscribeToVisibility,
    () => document.visibilityState !== "hidden",
    () => true,
  );
}

/**
 * Calls `onRefresh` once `AUTO_REFRESH_MS` after `since`, the instant of the last refresh (the
 * range's anchor, which every refresh moves), and again after each refresh. It waits while
 * `enabled` is false or the page is hidden; when it can run again and the interval has already
 * passed, the refresh comes at once. `onRefresh` is the page's own refresh: the timer decides only
 * when it runs.
 */
export function useAutoRefresh({
  enabled,
  since,
  onRefresh,
}: {
  enabled: boolean;
  since: number;
  onRefresh(): void;
}): void {
  const clock = useContext(AutoRefreshClockContext);
  const visible = usePageVisible();
  const latest = useRef(onRefresh);
  useEffect(() => {
    latest.current = onRefresh;
  });
  useEffect(() => {
    if (!enabled || !visible) return;
    return clock.after(Math.max(0, since + clock.intervalMs - Date.now()), () => latest.current());
  }, [enabled, visible, since, clock]);
}
