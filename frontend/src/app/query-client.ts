import { MutationCache, QueryCache, QueryClient } from "@tanstack/react-query";

import { ApiError, classifyError } from "../api/errors.ts";

/** How often a failed query is tried again, at most; only errors worth retrying are. */
export const MAX_RETRIES = 3;

/**
 * The query client every page uses. A request that comes back unauthenticated (the session
 * ended or was revoked) calls `onUnauthenticated`, which sends the user to sign in. Only the
 * errors the server marks retryable, and requests that got no response, are retried.
 */
export function createQueryClient(options: { onUnauthenticated: () => void }): QueryClient {
  const onError = (error: unknown) => {
    if (classifyError(error) === "unauthenticated") {
      options.onUnauthenticated();
    }
  };
  return new QueryClient({
    queryCache: new QueryCache({ onError }),
    mutationCache: new MutationCache({ onError }),
    defaultOptions: {
      queries: {
        retry: (failures, error) => failures < MAX_RETRIES && error instanceof ApiError && error.retryable,
        refetchOnWindowFocus: false,
      },
      mutations: { retry: false },
    },
  });
}
