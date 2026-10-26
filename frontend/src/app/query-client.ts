import { MutationCache, QueryCache, QueryClient } from "@tanstack/react-query";

import { ApiError, classifyError } from "../api/errors.ts";

/** How often a failed query is tried again, at most; only errors worth retrying are. */
export const MAX_RETRIES = 3;

/** What a query or mutation can tell the client's shared error handling. */
export interface RequestMeta extends Record<string, unknown> {
  /**
   * The caller answers a 401 itself instead of being sent to sign in: the sign-in form, where a
   * wrong code is a 401, and signing out everywhere, which says the other devices are still
   * signed in.
   */
  handlesUnauthenticated?: boolean;
  /** A tenant switch: its refusal is the switch's error, not a sign that the active tenant is suspended. */
  switchesTenant?: boolean;
}

declare module "@tanstack/react-query" {
  interface Register {
    queryMeta: RequestMeta;
    mutationMeta: RequestMeta;
  }
}

/** The meta of a request whose caller handles a 401 itself. */
export const HANDLES_UNAUTHENTICATED: RequestMeta = { handlesUnauthenticated: true };

/**
 * The query client every page uses. A request that comes back unauthenticated (the session
 * ended or was revoked) calls `onUnauthenticated`, which sends the user to sign in, unless the
 * request says it handles that itself. Only the errors the server marks retryable, and requests
 * that got no response, are retried.
 */
export function createQueryClient(options: { onUnauthenticated: () => void }): QueryClient {
  const onError = (error: unknown, meta: RequestMeta | undefined) => {
    if (classifyError(error) === "unauthenticated" && !meta?.handlesUnauthenticated) {
      options.onUnauthenticated();
    }
  };
  return new QueryClient({
    queryCache: new QueryCache({ onError: (error, query) => onError(error, query.meta) }),
    mutationCache: new MutationCache({
      onError: (error, _variables, _result, mutation) => onError(error, mutation.meta),
    }),
    defaultOptions: {
      queries: {
        retry: (failures, error) => failures < MAX_RETRIES && error instanceof ApiError && error.retryable,
        refetchOnWindowFocus: false,
      },
      mutations: { retry: false },
    },
  });
}
