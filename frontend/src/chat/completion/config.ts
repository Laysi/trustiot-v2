import { createContext, useContext } from "react";

/**
 * The timings and the clock of the composer's completion. The defaults are the design's; tests
 * give their own (a short delay to wait for, a fixed clock), so they assert the order of events
 * and never an elapsed time.
 */
export interface CompletionConfig {
  /** How long a lookup may take before the list shows placeholder rows (design 05-C: 300 ms; none flash when it is quicker). */
  skeletonDelayMs: number;
  /** How long the result count waits before it is read out, so one keystroke does not read it each time (design: 400 ms). */
  announceDelayMs: number;
  /** The clock the time ranges are worked out from. */
  now: () => number;
}

export const DEFAULT_COMPLETION_CONFIG: CompletionConfig = {
  skeletonDelayMs: 300,
  announceDelayMs: 400,
  now: () => Date.now(),
};

export const CompletionConfigContext = createContext<CompletionConfig>(DEFAULT_COMPLETION_CONFIG);

export function useCompletionConfig(): CompletionConfig {
  return useContext(CompletionConfigContext);
}
