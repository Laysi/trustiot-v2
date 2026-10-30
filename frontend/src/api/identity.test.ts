import { describe, expect, test } from "bun:test";

import { createApiClient } from "./client.ts";
import { ApiError, ResponseShapeError } from "./errors.ts";
import { identityApi } from "./identity.ts";

/** Answers each request with the next response, and keeps the requests with their bodies. */
function server(...responses: Response[]) {
  const requests: { method: string; path: string; body: unknown }[] = [];
  const fetch = async (request: Request) => {
    const text = await request.text();
    requests.push({
      method: request.method,
      path: new URL(request.url).pathname,
      body: text === "" ? undefined : JSON.parse(text),
    });
    const response = responses.shift();
    if (!response) throw new Error("no more responses");
    return response;
  };
  return { requests, api: identityApi(createApiClient({ fetch })) };
}

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

const noContent = () => new Response(null, { status: 204 });
const unauthenticated = () => json(401, { code: "unauthenticated", message_key: "error.unauthenticated" });
const ingress401 = () => new Response("unauthenticated", { status: 401 });

const SESSION = {
  user_id: "01928f6e-7a3c-7d4e-8f00-0000000000a1",
  email: "alice@example.com",
  platform_admin: false,
  active_tenant: {
    tenant_id: "01928f6e-7a3c-7d4e-8f00-0000000000b1",
    name: "Acme Factory",
    role: "admin",
    state: "active",
  },
  tenants: [
    { tenant_id: "01928f6e-7a3c-7d4e-8f00-0000000000b1", name: "Acme Factory", role: "admin", state: "active" },
    { tenant_id: "01928f6e-7a3c-7d4e-8f00-0000000000b2", name: null, role: "viewer", state: "suspended" },
  ],
};

describe("identityApi", () => {
  test("reads the session into the SPA's shape", async () => {
    const { api, requests } = server(json(200, SESSION));
    expect(await api.loadSession()).toEqual({
      user: { id: SESSION.user_id, email: "alice@example.com", platformAdmin: false },
      activeTenant: { id: "01928f6e-7a3c-7d4e-8f00-0000000000b1", name: "Acme Factory", role: "admin" },
      tenants: [
        { id: "01928f6e-7a3c-7d4e-8f00-0000000000b1", name: "Acme Factory", role: "admin" },
        { id: "01928f6e-7a3c-7d4e-8f00-0000000000b2", name: null, role: "viewer" },
      ],
    });
    expect(requests).toEqual([{ method: "GET", path: "/api/identity/v1/sessions/current", body: undefined }]);
  });

  test("no session is signed out, not a failure; the ingress's plain 401 too", async () => {
    expect(await server(unauthenticated()).api.loadSession()).toBeNull();
    expect(await server(ingress401()).api.loadSession()).toBeNull();
    await expect(server(new Response("down", { status: 503 })).api.loadSession()).rejects.toBeInstanceOf(ApiError);
  });

  test("a session body that does not fit, such as a role this page does not know, is a shape error", async () => {
    const body = { ...SESSION, active_tenant: { ...SESSION.active_tenant, role: "owner" } };
    const error = await server(json(200, body))
      .api.loadSession()
      .catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(ResponseShapeError);
    expect((error as ResponseShapeError).issues.map((issue) => issue.path)).toEqual(["/active_tenant/role"]);
  });

  test("switches the tenant with a JSON PUT and returns the session after the switch", async () => {
    const { api, requests } = server(json(200, SESSION));
    const session = await api.switchTenant("01928f6e-7a3c-7d4e-8f00-0000000000b1");
    expect(session.activeTenant?.name).toBe("Acme Factory");
    expect(requests).toEqual([
      {
        method: "PUT",
        path: "/api/identity/v1/sessions/current/tenant",
        body: { tenant_id: "01928f6e-7a3c-7d4e-8f00-0000000000b1" },
      },
    ]);
  });

  test("signing out of this browser resolves when the session had already ended; of every device it does not", async () => {
    const one = server(noContent(), unauthenticated());
    await one.api.logOut();
    await one.api.logOut();
    expect(one.requests.map((request) => `${request.method} ${request.path}`)).toEqual([
      "DELETE /api/identity/v1/sessions/current",
      "DELETE /api/identity/v1/sessions/current",
    ]);

    const all = server(noContent(), unauthenticated());
    await all.api.logOutEverywhere();
    const error = await all.api.logOutEverywhere().catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).status).toBe(401);
    expect(all.requests.map((request) => `${request.method} ${request.path}`)).toEqual([
      "DELETE /api/identity/v1/sessions",
      "DELETE /api/identity/v1/sessions",
    ]);
  });

  test("reads preferences and saves only the fields changed, in the API's names", async () => {
    const saved = { language: "zh-TW", ai_language: "en", time_zone: "Asia/Taipei", theme: "dark" };
    const { api, requests } = server(json(200, saved), json(200, { ...saved, ai_language: "auto" }));
    expect(await api.loadPreferences()).toEqual({
      language: "zh-TW",
      aiLanguage: "en",
      timeZone: "Asia/Taipei",
      theme: "dark",
    });
    expect(await api.updatePreferences({ aiLanguage: "auto" })).toEqual({
      language: "zh-TW",
      aiLanguage: "auto",
      timeZone: "Asia/Taipei",
      theme: "dark",
    });
    expect(requests).toEqual([
      { method: "GET", path: "/api/identity/v1/preferences", body: undefined },
      { method: "PATCH", path: "/api/identity/v1/preferences", body: { ai_language: "auto" } },
    ]);
  });

  test("a refused preference keeps its field errors", async () => {
    const { api } = server(
      json(422, {
        code: "validation_failed",
        message_key: "error.validation_failed",
        field_errors: [{ field: "/time_zone", message_key: "preferences.time_zone_invalid" }],
      }),
    );
    const error = (await api
      .updatePreferences({ timeZone: "Mars/Olympus" })
      .catch((caught: unknown) => caught)) as ApiError;
    expect(error.status).toBe(422);
    expect(error.body?.field_errors).toEqual([{ field: "/time_zone", message_key: "preferences.time_zone_invalid" }]);
  });

  test("sends the code and verifies it in two steps of the same endpoint", async () => {
    const { api, requests } = server(
      json(202, { code_length: 8, expires_in_secs: 600, resend_after_secs: 60 }),
      json(200, { redirect_to: "/devices?page=2" }),
    );
    expect(await api.sendCode("alice@example.com")).toEqual({ codeLength: 8, expiresInSecs: 600, resendAfterSecs: 60 });
    expect(
      await api.verifyCode({ email: "alice@example.com", code: "12345678", redirectTo: "/devices?page=2" }),
    ).toEqual({ redirectTo: "/devices?page=2" });
    expect(requests).toEqual([
      { method: "POST", path: "/auth/login", body: { step: "send_code", email: "alice@example.com" } },
      {
        method: "POST",
        path: "/auth/login",
        body: { step: "verify_code", email: "alice@example.com", code: "12345678", redirect_to: "/devices?page=2" },
      },
    ]);
  });

  test("each step's answer must be that step's: the other step's body is a shape error", async () => {
    const swapped = server(
      json(200, { redirect_to: "/" }),
      json(202, { code_length: 8, expires_in_secs: 600, resend_after_secs: 60 }),
    );
    expect(await swapped.api.sendCode("alice@example.com").catch((caught: unknown) => caught)).toBeInstanceOf(
      ResponseShapeError,
    );
    expect(
      await swapped.api
        .verifyCode({ email: "alice@example.com", code: "12345678", redirectTo: "/" })
        .catch((caught: unknown) => caught),
    ).toBeInstanceOf(ResponseShapeError);
  });

  test("a wrong code is a 401 the caller sees, not a signed-out session", async () => {
    const { api } = server(unauthenticated());
    const error = (await api
      .verifyCode({ email: "alice@example.com", code: "12345678", redirectTo: "/" })
      .catch((caught: unknown) => caught)) as ApiError;
    expect(error.status).toBe(401);
  });
});
