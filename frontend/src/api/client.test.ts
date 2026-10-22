import { describe, expect, test } from "bun:test";

import { createApiClient, unwrap } from "./client.ts";
import { ApiError, classifyError, parseErrorBody } from "./errors.ts";

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
    const tenant = await unwrap(client.GET("/api/assets/v1/tenant"));
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
    await unwrap(client.POST("/api/identity/v1/tenants", { body: { name: "Acme" } }));
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
    const error = await unwrap(client.POST("/api/identity/v1/tenants", { body: { name: "" } })).catch((e) => e);
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
    const error = await unwrap(createApiClient({ fetch }).GET("/api/assets/v1/tenant")).catch((e) => e);
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
    const error = await unwrap(client.GET("/api/assets/v1/tenant")).catch((e) => e);
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
    const error = await unwrap(client.GET("/api/assets/v1/tenant")).catch((e) => e);
    expect(error).not.toBeInstanceOf(ApiError);
    expect(error.name).toBe("AbortError");
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
    [500, { code: "internal", message_key: "error.internal" }, "other"],
  ] as const)("%i %o is %s", (status, body, kind) => {
    expect(classifyError(new ApiError(status, body))).toBe(kind);
  });

  test("only the codes the server marks retryable are retried", () => {
    expect(new ApiError(429, { code: "rate_limited", message_key: "error.rate_limited" }).retryable).toBe(true);
    expect(new ApiError(504, { code: "timeout", message_key: "error.timeout" }).retryable).toBe(true);
    expect(new ApiError(500, { code: "internal", message_key: "error.internal" }).retryable).toBe(false);
    expect(new ApiError(409, { code: "conflict", message_key: "error.conflict" }).retryable).toBe(false);
    expect(new ApiError(401, null).retryable).toBe(false);
  });
});
