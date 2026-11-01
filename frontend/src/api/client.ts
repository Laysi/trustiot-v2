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
 * The headers in which a request names the user and the tenant its page shows
 * (`contract::EXPECTED_USER_HEADER` and `EXPECTED_TENANT_HEADER`, Laysi/trustiot-v2#678), and the
 * tenant header's value for a page that shows none (`EXPECTED_NO_TENANT`). The cookie check
 * refuses a request whose user or tenant is not the session's with 409 `session_changed`: every
 * tab shares the session, so another tab may have switched its tenant or signed in as someone
 * else.
 */
export const EXPECTED_USER_HEADER = "trustiot-expected-user";
export const EXPECTED_TENANT_HEADER = "trustiot-expected-tenant";
export const EXPECTED_NO_TENANT = "none";

/**
 * The endpoints about the user or the session itself rather than the tenant a page shows. They
 * never name a tenant: reading the session is how a tab learns the tenant it is in, a switch names
 * the tenant it goes to, and signing in or out, or a preference, is the user's whatever the tenant.
 * Nor do the platform administration endpoints (`isPlatformPath`), which act on the tenant their
 * path or body names, never on the session's. Every other endpoint names one, so a new tenant
 * page is covered without doing anything.
 */
export const SESSION_PATHS: ReadonlySet<string> = new Set<keyof paths>([
  "/api/identity/v1/sessions/current",
  "/api/identity/v1/sessions/current/tenant",
  "/api/identity/v1/sessions",
  "/api/identity/v1/preferences",
  "/auth/login",
]);

/**
 * The endpoints that never name a user: reading the session is how a tab learns whose it is,
 * signing in makes one, and signing out (of this browser or everywhere) ends the browser's
 * session whoever it is now. Every other endpoint names the user, the platform's and a tenant
 * switch or preference change included, so a page of the previous user cannot act as the next.
 */
export const USERLESS_PATHS: ReadonlySet<string> = new Set<keyof paths>([
  "/api/identity/v1/sessions/current",
  "/api/identity/v1/sessions",
  "/auth/login",
]);

/** Who the pages show: the user's ID, and the tenant's (`null` for no tenant). */
export interface ShownSession {
  user: string;
  tenant: string | null;
}

/** Who the pages show now; `undefined` when they show no session (not read yet, or signed out). */
export type ShowsSession = () => ShownSession | undefined;

/** Creating a tenant, and every `/api/<service>/v1/platform/...` endpoint. */
const PLATFORM_TENANTS_PATH: keyof paths = "/api/identity/v1/tenants";
const PLATFORM_PATH = /^\/api\/[a-z0-9-]+\/v1\/platform\//;

/** Whether `schemaPath` is a platform administration endpoint, about no session's tenant. */
export function isPlatformPath(schemaPath: string): boolean {
  return schemaPath === PLATFORM_TENANTS_PATH || PLATFORM_PATH.test(schemaPath);
}

/**
 * The expected-session headers of a request to `schemaPath` from pages that show `shown`: the
 * user on every endpoint but `USERLESS_PATHS`, the tenant on every endpoint but `SESSION_PATHS`
 * and the platform's; nothing while the pages show no session.
 */
export function expectedSessionHeaders(shown: ShownSession | undefined, schemaPath: string): [string, string][] {
  if (shown === undefined) return [];
  const headers: [string, string][] = [];
  if (!USERLESS_PATHS.has(schemaPath)) headers.push([EXPECTED_USER_HEADER, shown.user]);
  if (!SESSION_PATHS.has(schemaPath) && !isPlatformPath(schemaPath)) {
    headers.push([EXPECTED_TENANT_HEADER, shown.tenant ?? EXPECTED_NO_TENANT]);
  }
  return headers;
}

/** Names the user and the tenant the pages show, when the request is sent (`expectedSessionHeaders`). */
export function expectedSession(shown: ShowsSession): Middleware {
  return {
    onRequest({ request, schemaPath }) {
      for (const [name, value] of expectedSessionHeaders(shown(), schemaPath)) request.headers.set(name, value);
      return request;
    },
  };
}

/**
 * The typed client for api/openapi.json. Requests go to the page's own origin (the SPA and the
 * API share it, decision 2) and send the session cookie only there: never `credentials:
 * "include"`. With `shownSession`, every request names the user and the tenant the pages show
 * (`expectedSession`).
 */
export function createApiClient(
  options: { fetch?: (request: Request) => Promise<Response>; shownSession?: ShowsSession } = {},
): ApiClient {
  const client = createClient<paths>({
    baseUrl: "",
    credentials: "same-origin",
    ...(options.fetch ? { fetch: options.fetch } : {}),
  });
  client.use(jsonWrites);
  if (options.shownSession) client.use(expectedSession(options.shownSession));
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
