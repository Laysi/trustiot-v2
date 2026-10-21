import createClient, { type Client, type Middleware } from "openapi-fetch";

import { ApiError, parseErrorBody } from "./errors.ts";
import type { paths } from "./schema.gen.ts";

export type ApiClient = Client<paths>;

/** Methods that never change state, and so need no CSRF protection of their own. */
const SAFE_METHODS: ReadonlySet<string> = new Set(["GET", "HEAD", "OPTIONS"]);

/**
 * The identity service accepts a cookie-authenticated write only as JSON
 * (services/identity/README.md): another site can make the browser send a form or text/plain
 * request with the cookie, but not a JSON one without a CORS preflight, which nothing answers.
 * So every write says application/json, also without a body.
 */
export const jsonWrites: Middleware = {
  onRequest({ request }) {
    request.headers.set("Accept", "application/json");
    if (!SAFE_METHODS.has(request.method) && !request.headers.has("Content-Type")) {
      request.headers.set("Content-Type", "application/json");
    }
    return request;
  },
};

/**
 * The typed client for api/openapi.json. Requests go to the page's own origin (the SPA and the
 * API share it, decision 2) and send the session cookie only there: never `credentials:
 * "include"`.
 */
export function createApiClient(options: { fetch?: (request: Request) => Promise<Response> } = {}): ApiClient {
  const client = createClient<paths>({
    baseUrl: "",
    credentials: "same-origin",
    ...(options.fetch ? { fetch: options.fetch } : {}),
  });
  client.use(jsonWrites);
  return client;
}

function isAbort(error: unknown): boolean {
  return error instanceof Error && error.name === "AbortError";
}

/**
 * The data of a successful call, or an `ApiError`: with the M1 body when the response carried
 * one, `status` 0 when no response arrived. An aborted request rethrows its abort error, so a
 * cancelled query is not reported as a failure.
 */
export async function unwrap<T>(pending: Promise<{ data?: T; error?: unknown; response: Response }>): Promise<T> {
  let result: { data?: T; error?: unknown; response: Response };
  try {
    result = await pending;
  } catch (cause) {
    if (isAbort(cause)) {
      throw cause;
    }
    throw new ApiError(0, null, { cause });
  }
  if (result.response.ok && result.error === undefined) {
    return result.data as T;
  }
  throw new ApiError(result.response.status, parseErrorBody(result.error));
}
