import { type ReactNode, useCallback } from "react";

import { type CompletionClock, CompletionProvider } from "../../chat/completion/context.tsx";
import type { CompletionApi } from "./completion.ts";

/**
 * Gives the composers grey-text completion: a fake in the UI preview and in tests. The app does
 * not mount it yet because the endpoint is pending (api/pending/completion.ts); the change that
 * replaces it mounts a provider over the real client in `AppProviders`.
 */
export function CompletionApiProvider({
  api,
  delayMs,
  clock,
  children,
}: {
  api: CompletionApi;
  /** The pause before a request is sent; the composer's default (250 ms) unless a test sets it. */
  delayMs?: number;
  clock?: CompletionClock;
  children: ReactNode;
}) {
  const complete = useCallback(
    async (text: string, signal: AbortSignal) => (await api.complete({ text }, { signal })).completion,
    [api],
  );
  return (
    <CompletionProvider
      complete={complete}
      {...(delayMs === undefined ? {} : { delayMs })}
      {...(clock ? { clock } : {})}
    >
      {children}
    </CompletionProvider>
  );
}
