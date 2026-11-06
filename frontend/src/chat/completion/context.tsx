import { createContext, type ReactNode, useContext, useMemo } from "react";

/**
 * Asks for the words that continue `text`; rejects when `signal` is aborted. An `ApiError` with
 * code `rate_limited` makes the composer stop asking until its `retry_after_secs` have passed.
 */
export type CompleteText = (text: string, signal: AbortSignal) => Promise<string>;

/** The timers the composer waits with; tests pass a clock they move by hand. */
export interface CompletionClock {
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
  now(): number;
}

export const browserClock: CompletionClock = {
  setTimeout: (callback, ms) => globalThis.setTimeout(callback, ms),
  clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
  now: () => Date.now(),
};

/** J11: the pause before a request is sent, between the 200 and 300 ms the checklist names. */
export const DEFAULT_COMPLETION_DELAY_MS = 250;

export interface CompletionSettings {
  complete: CompleteText;
  /** How long the user must stop typing before a request is sent. */
  delayMs: number;
  clock: CompletionClock;
}

/** `null`: this build has no completion, so the composer shows no grey text. */
const CompletionContext = createContext<CompletionSettings | null>(null);

/**
 * Turns grey-text completion on for the composers below. Whoever provides the completion
 * endpoint mounts it (`api/pending/CompletionApiProvider.tsx` in the UI preview; a provider over
 * the real client in `AppProviders` once the endpoint exists). It lives here, not next to the
 * pending API, because the product's composer reads it: the build must not carry
 * `src/api/pending/` (`check:dist`).
 */
export function CompletionProvider({
  complete,
  delayMs = DEFAULT_COMPLETION_DELAY_MS,
  clock = browserClock,
  children,
}: {
  complete: CompleteText;
  delayMs?: number;
  clock?: CompletionClock;
  children: ReactNode;
}) {
  const settings = useMemo(() => ({ complete, delayMs, clock }), [complete, delayMs, clock]);
  return <CompletionContext.Provider value={settings}>{children}</CompletionContext.Provider>;
}

export function useCompletionSettings(): CompletionSettings | null {
  return useContext(CompletionContext);
}
