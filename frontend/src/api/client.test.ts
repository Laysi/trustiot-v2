import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { createQueryClient } from "../app/query-client.ts";
import type { Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import {
  createApiClient,
  EXPECTED_NO_TENANT,
  EXPECTED_TENANT_HEADER,
  EXPECTED_USER_HEADER,
  isPlatformPath,
  SESSION_PATHS,
  type ShownSession,
  USERLESS_PATHS,
  unwrap,
} from "./client.ts";
import { ApiError, classifyError, parseErrorBody, ResponseShapeError } from "./errors.ts";
import type { components } from "./schema.gen.ts";

// Schemas as a screen writes them, bound to the generated types the way every schema is.
const AssetsTenant = z.object({ created_at: z.string(), tenant_id: z.string() });
export type AssetsTenantMatchesSpec = Assert<
  Equal<z.output<typeof AssetsTenant>, components["schemas"]["AssetsTenant"]>
>;
const Tenant = z.object({ created_at: z.string(), name: z.string(), tenant_id: z.string() });
export type TenantMatchesSpec = Assert<Equal<z.output<typeof Tenant>, components["schemas"]["Tenant"]>>;

const REPO_ROOT = fileURLToPath(new URL("../../../", import.meta.url));

function recordingFetch(respond: (request: Request) => Response | Promise<Response>) {
  const requests: Request[] = [];
  return {
    requests,
    fetch: async (request: Request) => {
      requests.push(request);
      return respond(request);
    },
  };
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

describe("createApiClient", () => {
  test("requests stay on the page's origin, send the cookie only there and ask for JSON", async () => {
    const { fetch, requests } = recordingFetch(() =>
      json(200, { tenant_id: "01928f6e-7a3c-7d4e-8f00-00000000000a", created_at: "2026-10-20T00:00:00Z" }),
    );
    const client = createApiClient({ fetch });
    const tenant = await unwrap(AssetsTenant, client.GET("/api/assets/v1/tenant"));
    expect(tenant.tenant_id).toBe("01928f6e-7a3c-7d4e-8f00-00000000000a");
    const request = requests[0] as Request;
    expect(new URL(request.url).origin).toBe(window.location.origin);
    expect(new URL(request.url).pathname).toBe("/api/assets/v1/tenant");
    expect(request.credentials).toBe("same-origin");
    expect(request.headers.get("accept")).toBe("application/json");
    expect(request.headers.get("content-type")).toBeNull();
  });

  test("every write is sent as JSON, which another site cannot send without a preflight", async () => {
    const { fetch, requests } = recordingFetch(() =>
      json(201, {
        tenant_id: "01928f6e-7a3c-7d4e-8f00-00000000000a",
        name: "Acme",
        created_at: "2026-10-20T00:00:00Z",
      }),
    );
    const client = createApiClient({ fetch });
    await unwrap(Tenant, client.POST("/api/identity/v1/tenants", { body: { name: "Acme" } }));
    const request = requests[0] as Request;
    expect(request.method).toBe("POST");
    expect(request.headers.get("content-type")).toBe("application/json");
    expect(await request.json()).toEqual({ name: "Acme" });
  });

  test("an M1 error body becomes an ApiError with the body", async () => {
    const { fetch } = recordingFetch(() =>
      json(422, {
        code: "validation_failed",
        message_key: "error.validation_failed",
        field_errors: [{ field: "/name", message_key: "tenant.name_invalid", details: { max: 100 } }],
        unknown_field: true,
      }),
    );
    const client = createApiClient({ fetch });
    const error = await unwrap(Tenant, client.POST("/api/identity/v1/tenants", { body: { name: "" } })).catch((e) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect(error.status).toBe(422);
    expect(error.body).toEqual({
      code: "validation_failed",
      message_key: "error.validation_failed",
      field_errors: [{ field: "/name", message_key: "tenant.name_invalid", details: { max: 100 } }],
    });
  });

  test("the ingress's plain-text 503 has no body and means unavailable, not signed out", async () => {
    const { fetch } = recordingFetch(() => new Response("no healthy upstream", { status: 503 }));
    const error = await unwrap(AssetsTenant, createApiClient({ fetch }).GET("/api/assets/v1/tenant")).catch((e) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect(error.body).toBeNull();
    expect(classifyError(error)).toBe("unavailable");
    expect(error.retryable).toBe(true);
  });

  test("no response at all is status 0, retryable", async () => {
    const client = createApiClient({
      fetch: async () => {
        throw new TypeError("Failed to fetch");
      },
    });
    const error = await unwrap(AssetsTenant, client.GET("/api/assets/v1/tenant")).catch((e) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect(error.status).toBe(0);
    expect(error.retryable).toBe(true);
  });

  test("an aborted request rethrows the abort, not an ApiError", async () => {
    const client = createApiClient({
      fetch: async () => {
        throw new DOMException("aborted", "AbortError");
      },
    });
    const error = await unwrap(AssetsTenant, client.GET("/api/assets/v1/tenant")).catch((e) => e);
    expect(error).not.toBeInstanceOf(ApiError);
    expect(error.name).toBe("AbortError");
  });
});

describe("the expected user and tenant (Laysi/trustiot-v2#678)", () => {
  const USER = "01928f6e-7a3c-7d4e-8f00-0000000000a1";
  const OTHER_USER = "01928f6e-7a3c-7d4e-8f00-0000000000b2";
  const TENANT = "01928f6e-7a3c-7d4e-8f00-00000000000a";
  const ok = () => new Response(null, { status: 204 });
  const named = (request: Request) => [
    request.headers.get(EXPECTED_USER_HEADER),
    request.headers.get(EXPECTED_TENANT_HEADER),
  ];

  test("every request about a tenant names the user and the tenant the pages show, at the time it is sent", async () => {
    const { fetch, requests } = recordingFetch(ok);
    let shown: ShownSession | undefined = { user: USER, tenant: TENANT };
    const client = createApiClient({ fetch, shownSession: () => shown });
    await client.GET("/api/assets/v1/tenant");
    await client.POST("/api/assistant/v1/conversations", { body: {} });
    await client.DELETE("/api/assets/v1/devices/{device_id}", { params: { path: { device_id: TENANT } } });
    await client.PATCH("/api/assistant/v1/conversations/{conversation_id}", {
      params: { path: { conversation_id: TENANT } },
      body: { title: "Line 3" },
    });
    shown = { user: OTHER_USER, tenant: null };
    await client.POST("/api/assistant/v1/conversations", { body: {} });
    shown = undefined;
    await client.POST("/api/assistant/v1/conversations", { body: {} });
    expect(requests.map(named)).toEqual([
      [USER, TENANT],
      [USER, TENANT],
      [USER, TENANT],
      [USER, TENANT],
      [OTHER_USER, EXPECTED_NO_TENANT],
      [null, null],
    ]);
  });

  test("requests about the session or the user name no tenant, and only reading, signing in or out name no user", async () => {
    const { fetch, requests } = recordingFetch(ok);
    const client = createApiClient({ fetch, shownSession: () => ({ user: USER, tenant: TENANT }) });
    await client.GET("/api/identity/v1/sessions/current");
    await client.DELETE("/api/identity/v1/sessions/current");
    await client.DELETE("/api/identity/v1/sessions");
    await client.POST("/auth/login", { body: { step: "send_code", email: "alice@example.com" } });
    await client.PUT("/api/identity/v1/sessions/current/tenant", { body: { tenant_id: TENANT } });
    await client.GET("/api/identity/v1/preferences");
    await client.PATCH("/api/identity/v1/preferences", { body: { theme: "dark" } });
    expect(requests.map(named)).toEqual([
      [null, null],
      [null, null],
      [null, null],
      [null, null],
      [USER, null],
      [USER, null],
      [USER, null],
    ]);
    expect([...SESSION_PATHS].sort()).toEqual([
      "/api/identity/v1/preferences",
      "/api/identity/v1/sessions",
      "/api/identity/v1/sessions/current",
      "/api/identity/v1/sessions/current/tenant",
      "/auth/login",
    ]);
    expect([...USERLESS_PATHS].sort()).toEqual([
      "/api/identity/v1/sessions",
      "/api/identity/v1/sessions/current",
      "/auth/login",
    ]);
  });

  test("platform administration requests name the user but no tenant: they act on the one their path or body names", async () => {
    const { fetch, requests } = recordingFetch(ok);
    const client = createApiClient({ fetch, shownSession: () => ({ user: USER, tenant: TENANT }) });
    await client.PUT("/api/identity/v1/platform/tenants/{tenant_id}/state", {
      params: { path: { tenant_id: TENANT } },
      body: { state: "active" },
    });
    await client.GET("/api/assets/v1/platform/gateways");
    await client.POST("/api/identity/v1/tenants", { body: { name: "Gamma Works" } });
    expect(requests.map(named)).toEqual([
      [USER, null],
      [USER, null],
      [USER, null],
    ]);
    expect(isPlatformPath("/api/assets/v1/devices")).toBe(false);
    expect(isPlatformPath("/api/identity/v1/platform/event-log")).toBe(true);
  });

  test("without shownSession no request names a user or a tenant", async () => {
    const { fetch, requests } = recordingFetch(ok);
    await createApiClient({ fetch }).POST("/api/assistant/v1/conversations", { body: {} });
    expect(requests[0]?.headers.has(EXPECTED_USER_HEADER)).toBe(false);
    expect(requests[0]?.headers.has(EXPECTED_TENANT_HEADER)).toBe(false);
  });

  test("the headers and the no-tenant value are the ones the cookie check reads (crates/contract)", () => {
    const source = readFileSync(join(REPO_ROOT, "crates", "contract", "src", "identity.rs"), "utf8");
    expect(source).toContain(`pub const EXPECTED_USER_HEADER: &str = "${EXPECTED_USER_HEADER}";`);
    expect(source).toContain(`pub const EXPECTED_TENANT_HEADER: &str = "${EXPECTED_TENANT_HEADER}";`);
    expect(source).toContain(`pub const EXPECTED_NO_TENANT: &str = "${EXPECTED_NO_TENANT}";`);
  });
});

describe("unwrap parses the response body", () => {
  const SECRET = "secret-value-7f3a";

  test("a 200 that lacks a required field is a ResponseShapeError naming the path, not the value", async () => {
    const { fetch } = recordingFetch(() => json(200, { name: SECRET, created_at: { nested: SECRET } }));
    const error = await unwrap(
      Tenant,
      createApiClient({ fetch }).POST("/api/identity/v1/tenants", { body: { name: "x" } }),
    ).catch((e) => e);
    expect(error).toBeInstanceOf(ResponseShapeError);
    expect(error).not.toBeInstanceOf(ApiError);
    expect(error.status).toBe(200);
    expect(error.issues).toEqual([
      { path: "/created_at", code: "invalid_type" },
      { path: "/tenant_id", code: "invalid_type" },
    ]);
    expect(error.message).toBe(
      "HTTP 200: response does not match its schema: /created_at invalid_type, /tenant_id invalid_type",
    );
    expect(`${error.message} ${JSON.stringify(error.issues)} ${error.stack}`).not.toContain(SECRET);
  });

  test("it is classified as other and a query does not retry it", async () => {
    const { fetch } = recordingFetch(() => json(200, { unexpected: SECRET }));
    const client = createApiClient({ fetch });
    const queryClient = createQueryClient({ onUnauthenticated: () => {} });
    let calls = 0;
    const error = await queryClient
      .fetchQuery({
        queryKey: ["tenant"],
        queryFn: () => {
          calls += 1;
          return unwrap(AssetsTenant, client.GET("/api/assets/v1/tenant"));
        },
      })
      .catch((e) => e);
    expect(error).toBeInstanceOf(ResponseShapeError);
    expect(classifyError(error)).toBe("other");
    expect(calls).toBe(1);

    const retry = queryClient.getDefaultOptions().queries?.retry;
    if (typeof retry !== "function") throw new Error("the query client has no retry function");
    expect(retry(0, error)).toBe(false);
    // The same function does retry what the server marks retryable, so the answer above is its own.
    expect(retry(0, new ApiError(503, null))).toBe(true);
  });

  test("an empty 200 where a body is expected is a ResponseShapeError for the whole body", async () => {
    const { fetch } = recordingFetch(() => new Response(null, { status: 200 }));
    const error = await unwrap(AssetsTenant, createApiClient({ fetch }).GET("/api/assets/v1/tenant")).catch((e) => e);
    expect(error).toBeInstanceOf(ResponseShapeError);
    expect(error.issues).toEqual([{ path: "", code: "invalid_type" }]);
    expect(error.message).toContain("(body) invalid_type");
  });

  test("a 204 is parsed with z.undefined()", async () => {
    const { fetch } = recordingFetch(() => new Response(null, { status: 204 }));
    const client = createApiClient({ fetch });
    const call = () =>
      client.DELETE("/api/assets/v1/device-models/{model_id}", { params: { path: { model_id: "m-1" } } });
    expect(await unwrap(z.undefined(), call())).toBeUndefined();
    expect(await unwrap(AssetsTenant, call()).catch((e) => e)).toBeInstanceOf(ResponseShapeError);
  });

  test("the result is the schema's output: members it does not name are dropped", async () => {
    const { fetch } = recordingFetch(() =>
      json(200, { tenant_id: "t-1", created_at: "2026-10-20T00:00:00Z", extra: 1 }),
    );
    const tenant = await unwrap(AssetsTenant, createApiClient({ fetch }).GET("/api/assets/v1/tenant"));
    expect(tenant).toEqual({ tenant_id: "t-1", created_at: "2026-10-20T00:00:00Z" });
  });

  test("an error response is an ApiError and its body is not parsed with the success schema", async () => {
    const { fetch } = recordingFetch(() => json(404, { code: "not_found", message_key: "error.not_found" }));
    const error = await unwrap(AssetsTenant, createApiClient({ fetch }).GET("/api/assets/v1/tenant")).catch((e) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect(error.body).toEqual({ code: "not_found", message_key: "error.not_found" });
  });
});

describe("parseErrorBody", () => {
  test("rejects what is not an M1 body", () => {
    expect(parseErrorBody("upstream connect error")).toBeNull();
    expect(parseErrorBody(null)).toBeNull();
    expect(parseErrorBody({ code: "teapot", message_key: "error.teapot" })).toBeNull();
    expect(parseErrorBody({ code: "conflict" })).toBeNull();
  });

  test("keeps only text and numbers in details", () => {
    expect(
      parseErrorBody({
        code: "quota_exceeded",
        message_key: "error.quota_exceeded",
        details: { max: 5, html: { x: 1 }, s: "a" },
      }),
    ).toEqual({ code: "quota_exceeded", message_key: "error.quota_exceeded", details: { max: 5, s: "a" } });
  });

  test("drops numbers that are not finite, such as the 1e400 JSON.parse turns into Infinity", () => {
    const body = JSON.parse(
      '{"code":"quota_exceeded","message_key":"error.quota_exceeded","details":{"max":5,"big":1e400}}',
    );
    expect(body.details.big).toBe(Number.POSITIVE_INFINITY);
    expect(parseErrorBody(body)).toStrictEqual({
      code: "quota_exceeded",
      message_key: "error.quota_exceeded",
      details: { max: 5 },
    });
  });

  test("ignores unknown fields and leaves out details and field errors that come to nothing", () => {
    for (const odd of [{}, { max: { x: 1 } }, "text", 5, null, ["a"]]) {
      expect(
        parseErrorBody({ code: "conflict", message_key: "error.conflict", details: odd, field_errors: [], future: 1 }),
      ).toStrictEqual({ code: "conflict", message_key: "error.conflict" });
    }
    expect(parseErrorBody({ code: "conflict", message_key: "error.conflict", field_errors: "x" })).toStrictEqual({
      code: "conflict",
      message_key: "error.conflict",
    });
  });

  test("drops the field errors that are not field errors, and cleans the details of the rest", () => {
    expect(
      parseErrorBody({
        code: "validation_failed",
        message_key: "error.validation_failed",
        field_errors: [
          null,
          "x",
          { field: "/a" },
          { field: 1, message_key: "k.a" },
          { field: "/b", message_key: "k.b", details: { max: 3, bad: [1], inf: null }, other: true },
          { field: "/c", message_key: "k.c", details: {} },
        ],
      }),
    ).toStrictEqual({
      code: "validation_failed",
      message_key: "error.validation_failed",
      field_errors: [
        { field: "/b", message_key: "k.b", details: { max: 3 } },
        { field: "/c", message_key: "k.c" },
      ],
    });
  });
});

describe("classifyError and retryable", () => {
  test.each([
    [401, null, "unauthenticated"],
    [403, null, "forbidden"],
    [404, null, "not_found"],
    [503, null, "unavailable"],
    [504, null, "unavailable"],
    [403, { code: "tenant_suspended", message_key: "error.tenant_suspended" }, "tenant_suspended"],
    [403, { code: "permission_denied", message_key: "security.cross_site_request" }, "forbidden"],
    [409, { code: "conflict", message_key: "error.conflict" }, "other"],
    [409, { code: "session_changed", message_key: "error.session_changed" }, "session_changed"],
    [500, { code: "internal", message_key: "error.internal" }, "other"],
  ] as const)("%i %o is %s", (status, body, kind) => {
    expect(classifyError(new ApiError(status, body))).toBe(kind);
  });

  test("session_changed is parsed and not retried: the same request names the same user and tenant again", () => {
    const body = parseErrorBody({ code: "session_changed", message_key: "error.session_changed" });
    expect(body).toEqual({ code: "session_changed", message_key: "error.session_changed" });
    expect(new ApiError(409, body).retryable).toBe(false);
  });

  test("only the codes the server marks retryable are retried", () => {
    expect(new ApiError(429, { code: "rate_limited", message_key: "error.rate_limited" }).retryable).toBe(true);
    expect(new ApiError(504, { code: "timeout", message_key: "error.timeout" }).retryable).toBe(true);
    expect(new ApiError(500, { code: "internal", message_key: "error.internal" }).retryable).toBe(false);
    expect(new ApiError(409, { code: "conflict", message_key: "error.conflict" }).retryable).toBe(false);
    expect(new ApiError(401, null).retryable).toBe(false);
  });
});
