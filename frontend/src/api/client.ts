import createClient, { type Client, type Middleware } from "openapi-fetch";

import type { z } from "../lib/zod.ts";
import { ApiError, parseErrorBody, ResponseShapeError } from "./errors.ts";
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

/** What openapi-fetch resolves a call to: `data` for a success, `error` for an error response. */
interface Result<T> {
  data?: T;
  error?: unknown;
  response: Response;
}

/**
 * The data of a successful call, parsed with `schema`, or an error. The generated types only
 * claim what the server sends, at compile time; the server can be another version than this
 * page, so the body is checked here and the result has the type of `schema`'s output:
 *
 * - `ApiError` when the call failed: with the M1 body when the response carried one, `status` 0
 *   when no response arrived.
 * - `ResponseShapeError` when a 2xx response does not fit `schema`.
 *
 * An aborted request rethrows its abort error, so a cancelled query is not reported as a
 * failure. A response without a body (204) has no data: its schema is `z.undefined()`.
 *
 * The schema's output type has to cover the endpoint's success type, which catches a schema
 * written for another endpoint; that it is the same type is what `Equal` next to the schema
 * checks (lib/type-binding.ts).
 */
export async function unwrap<T>(schema: z.ZodType<T>, pending: Promise<Result<NoInfer<T>>>): Promise<T> {
  let result: Result<T>;
  try {
    result = await pending;
  } catch (cause) {
    if (isAbort(cause)) {
      throw cause;
    }
    throw new ApiError(0, null, { cause });
  }
  if (result.response.ok && result.error === undefined) {
    const parsed = schema.safeParse(result.data);
    if (!parsed.success) {
      throw ResponseShapeError.from(result.response.status, parsed.error);
    }
    return parsed.data;
  }
  throw new ApiError(result.response.status, parseErrorBody(result.error));
}
