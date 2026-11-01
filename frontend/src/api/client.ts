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
 * signing in makes one, and signing out of this browser ends the browser's session whoever it is
 * now, which only ever leaves someone with one session fewer. Every other endpoint names the user:
 * the platform's, a tenant switch, a preference change, and signing out everywhere, which ends
 * every session of the user it names, so a page of the previous user cannot end the next one's.
 * The path is the same for reading and ending the current session, and neither names a user.
 */
export const USERLESS_PATHS: ReadonlySet<string> = new Set<keyof paths>([
  "/api/identity/v1/sessions/current",
  "/auth/login",
]);

/** Who the pages show: the user's ID, and the tenant's (`null` for no tenant). */
export interface ShownSession {
  user: string;
  tenant: string | null;
}

/** Who the pages show now; `undefined` when they show no session (not read yet, or signed out). */
export type ShowsSession = () => ShownSession | undefined;

/**
 * What a page was mounted for (`PinSession`): the user, and under `TenantGate` the tenant. A
 * request the page makes names these, not who the session shows when it is sent, so a request a
 * page sends late (after an `await`, or as a later step of a mutation) still names its own page's
 * user and tenant and is refused once they changed.
 */
export interface PinnedSession {
  user?: string;
  tenant?: string | null;
}

/** Creating a tenant, and every `/api/<service>/v1/platform/...` endpoint. */
const PLATFORM_TENANTS_PATH: keyof paths = "/api/identity/v1/tenants";
const PLATFORM_PATH = /^\/api\/[a-z0-9-]+\/v1\/platform\//;

/** Whether `schemaPath` is a platform administration endpoint, about no session's tenant. */
export function isPlatformPath(schemaPath: string): boolean {
  return schemaPath === PLATFORM_TENANTS_PATH || PLATFORM_PATH.test(schemaPath);
}

/** The pinned user and tenant where a page pinned them, who the pages show now otherwise. */
export function expectedOf(
  live: ShownSession | undefined,
  pinned: PinnedSession | undefined,
): PinnedSession | undefined {
  if (pinned === undefined) return live;
  return {
    ...live,
    ...(pinned.user !== undefined ? { user: pinned.user } : {}),
    ...(pinned.tenant !== undefined ? { tenant: pinned.tenant } : {}),
  };
}

/**
 * The expected-session headers of a request to `schemaPath` naming `expected`: the user on every
 * endpoint but `USERLESS_PATHS`, the tenant on every endpoint but `SESSION_PATHS` and the
 * platform's; nothing while there is no session to name.
 */
export function expectedSessionHeaders(expected: PinnedSession | undefined, schemaPath: string): [string, string][] {
  const headers: [string, string][] = [];
  if (expected?.user !== undefined && !USERLESS_PATHS.has(schemaPath))
    headers.push([EXPECTED_USER_HEADER, expected.user]);
  if (expected?.tenant !== undefined && !SESSION_PATHS.has(schemaPath) && !isPlatformPath(schemaPath)) {
    headers.push([EXPECTED_TENANT_HEADER, expected.tenant ?? EXPECTED_NO_TENANT]);
  }
  return headers;
}

/** Where the client, and an API object made with `pinnable`, keep how to make a pinned copy. */
const PIN: unique symbol = Symbol("pin session");

interface Pinnable<T> {
  [PIN]?: (pinned: PinnedSession) => T;
}

/**
 * `target` bound to a page: a copy whose every request names `pinned` (and, for what `pinned`
 * leaves out, who the pages show when the request is sent), however late it is sent, after any
 * number of `await`s. `target` is the client or an API object made with `pinnable`; anything else
 * (a test's hand-written fake, which sends no request) is returned as it is.
 */
export function pinSession<T extends object>(target: T, pinned: PinnedSession): T {
  const pin = (target as Pinnable<T>)[PIN];
  return typeof pin === "function" ? pin(pinned) : target;
}

/**
 * The API object `make` builds on `client`, which `pinSession` builds again on a pinned copy of
 * the client, so every request of every method names the page's user and tenant.
 */
export function pinnable<T extends object>(client: ApiClient, make: (client: ApiClient) => T): T {
  const pin = (pinned: PinnedSession) => make(pinSession(client, pinned));
  return Object.assign(make(client), { [PIN]: pin });
}

const CLIENT_METHODS: ReadonlySet<string> = new Set([
  "GET",
  "PUT",
  "POST",
  "DELETE",
  "OPTIONS",
  "HEAD",
  "PATCH",
  "TRACE",
]);

/**
 * Names the user and the tenant of each request (`expectedSessionHeaders`): `pinned` where it
 * names them, else who the pages show (`shown`) when the request is sent. The headers are set by
 * the request's own openapi-fetch middleware, and `pinSession` makes a copy with another `pinned`
 * over the same client.
 */
function expectingSession(client: ApiClient, shown: ShowsSession | undefined, pinned?: PinnedSession): ApiClient {
  const expected: Middleware = {
    onRequest({ request, schemaPath }) {
      for (const [name, value] of expectedSessionHeaders(expectedOf(shown?.(), pinned), schemaPath)) {
        request.headers.set(name, value);
      }
      return request;
    },
  };
  const pin = (next: PinnedSession) => expectingSession(client, shown, { ...pinned, ...next });
  return new Proxy(client, {
    get(object, property, receiver) {
      if (property === PIN) return pin;
      const value: unknown = Reflect.get(object, property, receiver);
      if (typeof property !== "string" || !CLIENT_METHODS.has(property) || typeof value !== "function") return value;
      return (path: unknown, init?: { middleware?: Middleware[] }) =>
        Reflect.apply(value, object, [path, { ...init, middleware: [...(init?.middleware ?? []), expected] }]);
    },
  });
}

/**
 * The typed client for api/openapi.json. Requests go to the page's own origin (the SPA and the
 * API share it, decision 2) and send the session cookie only there: never `credentials:
 * "include"`. Every request names the user and the tenant of the page that made it
 * (`expectingSession`): pinned by the page (`PinSession`), or else the ones `shownSession` gives.
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
  return expectingSession(client, options.shownSession);
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
