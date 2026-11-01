// The ingress stand-in of the browser end-to-end run, against fake identity and web services: the
// routing and the ext_authz handling it must do the way the cluster's ingress does.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import { type Ingress, startIngress } from "./ingress.ts";

interface Seen {
  method: string;
  path: string;
  host: string | null;
  cookie: string | null;
  identity: string | null;
  fetchSite: string | null;
  body: string;
}

const seenByIdentity: Seen[] = [];
const seenByWeb: Seen[] = [];

async function seen(request: Request): Promise<Seen> {
  const url = new URL(request.url);
  return {
    method: request.method,
    path: url.pathname + url.search,
    host: request.headers.get("host"),
    cookie: request.headers.get("cookie"),
    identity: request.headers.get("x-internal-identity"),
    fetchSite: request.headers.get("sec-fetch-site"),
    body: await request.text(),
  };
}

const identityServer = Bun.serve({
  port: 0,
  hostname: "127.0.0.1",
  async fetch(request) {
    seenByIdentity.push(await seen(request));
    const url = new URL(request.url);
    if (url.pathname.startsWith("/internal/ext-authz")) {
      const cookie = request.headers.get("cookie") ?? "";
      if (cookie.includes("good")) return new Response(null, { status: 200, headers: { "x-internal-identity": "token-for-good" } });
      return Response.json({ error: "unauthenticated" }, { status: 401 });
    }
    // Signing out answers 204: no body, which the stand-in has to pass on as it is.
    if (request.method === "DELETE") return new Response(null, { status: 204 });
    return Response.json({ answered: url.pathname }, { status: url.pathname === "/auth/login" ? 202 : 200, headers: { "set-cookie": "a=1" } });
  },
});
const webServer = Bun.serve({
  port: 0,
  hostname: "127.0.0.1",
  async fetch(request) {
    seenByWeb.push(await seen(request));
    return new Response("<html></html>", { headers: { "content-type": "text/html", "content-security-policy": "default-src 'none'" } });
  },
});

let ingress: Ingress;
let base: string;

beforeAll(() => {
  ingress = startIngress({ identity: `http://127.0.0.1:${identityServer.port}`, web: `http://127.0.0.1:${webServer.port}` });
  base = `http://127.0.0.1:${ingress.port}`;
});
afterAll(async () => {
  await ingress.close();
  await identityServer.stop(true);
  await webServer.stop(true);
});

describe("the ingress stand-in", () => {
  test("sends /auth/login straight to identity, with the route table's host", async () => {
    const response = await fetch(`${base}/auth/login`, { method: "POST", body: '{"step":"send_code"}', headers: { "x-internal-identity": "forged" } });
    expect(response.status).toBe(202);
    const call = seenByIdentity.at(-1);
    expect(call).toMatchObject({ method: "POST", path: "/auth/login", host: "localhost", identity: null, body: '{"step":"send_code"}' });
  });

  test("answers a cookie route the way ext_authz did when it did not say 200", async () => {
    const response = await fetch(`${base}/api/identity/v1/sessions/current`, { headers: { cookie: "s=bad" } });
    expect(response.status).toBe(401);
    expect(await response.json()).toEqual({ error: "unauthenticated" });
    // Only the check reached the identity service, not the request.
    expect(seenByIdentity.at(-1)?.path).toBe("/internal/ext-authz/api/identity/v1/sessions/current");
  });

  test("passes a checked request on with the token ext_authz returned, never the client's", async () => {
    const response = await fetch(`${base}/api/identity/v1/preferences?x=1`, {
      method: "PATCH",
      body: '{"theme":"dark"}',
      headers: { cookie: "s=good", "x-internal-identity": "forged", "sec-fetch-site": "same-origin" },
    });
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ answered: "/api/identity/v1/preferences" });
    const [check, call] = seenByIdentity.slice(-2);
    expect(check).toMatchObject({ method: "PATCH", path: "/internal/ext-authz/api/identity/v1/preferences?x=1", fetchSite: "same-origin", cookie: "s=good" });
    expect(call).toMatchObject({ method: "PATCH", path: "/api/identity/v1/preferences?x=1", identity: "token-for-good", body: '{"theme":"dark"}' });
  });

  test("passes an answer without a body (204) on as it is", async () => {
    const response = await fetch(`${base}/api/identity/v1/sessions/current`, { method: "DELETE", headers: { cookie: "s=good" } });
    expect(response.status).toBe(204);
    expect(await response.text()).toBe("");
  });

  test("does not route any other /api, /auth or /internal path", async () => {
    for (const path of ["/api/other", "/api", "/auth/other", "/internal/ext-authz/api/x", "/api/identity-not"]) {
      expect((await fetch(`${base}${path}`)).status, path).toBe(404);
    }
  });

  test("sends everything else to web and keeps its headers", async () => {
    const response = await fetch(`${base}/settings`, { headers: { cookie: "s=good" } });
    expect(response.status).toBe(200);
    expect(response.headers.get("content-security-policy")).toBe("default-src 'none'");
    expect(seenByWeb.at(-1)).toMatchObject({ path: "/settings", host: "localhost" });
  });

  test("logs the status of every /api and /auth request, and nothing else", () => {
    expect(ingress.log()).toEqual([
      "POST /auth/login 202",
      "GET /api/identity/v1/sessions/current 401",
      "PATCH /api/identity/v1/preferences 200",
      "DELETE /api/identity/v1/sessions/current 204",
      "GET /api/other 404",
      "GET /api 404",
      "GET /auth/other 404",
      "GET /api/identity-not 404",
    ]);
  });
});
