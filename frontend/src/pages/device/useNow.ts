import { useEffect, useState } from "react";

/** How often the page re-reads the clock, so 「12 seconds ago」, the stalled state and the open end of a chart do not freeze. */
export const CLOCK_TICK_MS = 30_000;

/** The current time, moving on every `intervalMs`. */
export function useNow(intervalMs: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(timer);
  }, [intervalMs]);
  return now;
}
