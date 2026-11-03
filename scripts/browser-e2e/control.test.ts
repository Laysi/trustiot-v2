// The control surface of the browser end-to-end run: who may call it, what it accepts, and what it
// asks the database to do (the statements run as the schema owner and take their values as psql
// variables, never as text in the script).
import { createHash } from "node:crypto";
import { describe, expect, test } from "bun:test";

import { cookieHashHex } from "../lib/identity.ts";
import {
  ADD_MEMBERSHIP_SQL,
  CREATE_SESSION_SQL,
  createControl,
  createUserScript,
  type Database,
  LIVE_SESSIONS_SQL,
  newToken,
  SEED_SQL,
  TENANTS,
} from "./control.ts";
import type { Mail } from "./smtp.ts";

/** The run's bearer token: random, as in a real run. */
const TOKEN = newToken();

interface Call {
  script: string;
  variables: Record<string, string>;
}

function setup(answer = "x\n", mails: Mail[] = []) {
  const calls: Call[] = [];
  const database: Database = {
    async run(script, variables) {
      calls.push({ script, variables });
      return answer;
    },
  };
  const handler = createControl({ database, mails: () => mails, token: TOKEN });
  const call = (method: string, path: string, body?: unknown, token: string | null = TOKEN) =>
    handler(
      new Request(`http://control.test${path}`, {
        method,
        headers: { ...(token === null ? {} : { authorization: `Bearer ${token}` }), "content-type": "application/json" },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      }),
    );
  return { calls, call };
}

describe("who may call", () => {
  test("a call without the run's token, or with another, is refused before anything is done", async () => {
    const { calls, call } = setup();
    expect((await call("POST", "/users", { email: "a@example.com", memberships: [] }, null)).status).toBe(401);
    expect((await call("POST", "/users", { email: "a@example.com", memberships: [] }, "other")).status).toBe(401);
    expect((await call("GET", "/mail?to=a@example.com", undefined, "")).status).toBe(401);
    expect(calls).toEqual([]);
  });
});

describe("what it accepts", () => {
  test("only lowercase addresses on example.com, so no statement can be shaped by an address", async () => {
    const { calls, call } = setup();
    for (const email of ["A@example.com", "a@evil.test", "a'; DROP TABLE x;--@example.com", "a b@example.com", "", 5, null]) {
      const response = await call("POST", "/users", { email, memberships: [] });
      expect(response.status, String(email)).toBe(422);
    }
    expect(calls).toEqual([]);
  });

  test("only the run's two tenants and the three roles", async () => {
    const { calls, call } = setup();
    expect((await call("POST", "/users", { email: "a@example.com", memberships: [{ tenant: "gamma", role: "admin" }] })).status).toBe(422);
    expect((await call("POST", "/users", { email: "a@example.com", memberships: [{ tenant: "acme", role: "root" }] })).status).toBe(422);
    expect((await call("POST", "/memberships", { email: "a@example.com", tenant: "acme", role: "x" })).status).toBe(422);
    expect((await call("POST", "/sessions", { email: "a@example.com", tenant: "00000000-0000-4000-8000-000000000000" })).status).toBe(422);
    expect(calls).toEqual([]);
  });

  test("an unknown route is a 404 and a body that is not an object a 400", async () => {
    const { call } = setup();
    expect((await call("GET", "/nothing")).status).toBe(404);
    expect((await call("POST", "/users", [])).status).toBe(400);
  });
});

describe("what it asks of the database", () => {
  test("a user and their memberships, with the ids as variables", async () => {
    const { calls, call } = setup();
    const response = await call("POST", "/users", {
      email: "alice@example.com",
      memberships: [
        { tenant: "acme", role: "admin" },
        { tenant: "beta", role: "viewer" },
      ],
    });
    expect(response.status).toBe(201);
    const { user_id } = (await response.json()) as { user_id: string };
    const [first] = calls;
    expect(first?.variables).toEqual({
      user_id,
      email: "alice@example.com",
      tenant0: TENANTS.acme.id,
      role0: "admin",
      tenant1: TENANTS.beta.id,
      role1: "viewer",
    });
    expect(first?.script).toBe(createUserScript([{ tenant: "acme", role: "admin" }, { tenant: "beta", role: "viewer" }]));
    expect(first?.script).not.toContain("alice");
  });

  test("a session stores only the SHA-256 of the cookie's bytes, and the cookie goes back to the caller", async () => {
    const { calls, call } = setup();
    const response = await call("POST", "/sessions", { email: "alice@example.com", tenant: "beta" });
    expect(response.status).toBe(201);
    const { cookie } = (await response.json()) as { cookie: string };
    expect(cookie).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(calls[0]?.variables.hash).toBe(createHash("sha256").update(Buffer.from(cookie, "base64url")).digest("hex"));
    expect(calls[0]?.variables.hash).toBe(cookieHashHex(cookie));
    expect(calls[0]?.variables.tenant).toBe(TENANTS.beta.id);
    expect(Object.values(calls[0]?.variables ?? {})).not.toContain(cookie);
  });

  test("a session without a tenant has an empty tenant variable", async () => {
    const { calls, call } = setup();
    await call("POST", "/sessions", { email: "alice@example.com" });
    expect(calls[0]?.variables.tenant).toBe("");
  });

  test("a user who does not exist is a 404, not a session", async () => {
    const { call } = setup("");
    expect((await call("POST", "/sessions", { email: "nobody@example.com" })).status).toBe(404);
    expect((await call("POST", "/memberships", { email: "nobody@example.com", tenant: "acme", role: "viewer" })).status).toBe(404);
  });

  test("the live sessions are counted from the database", async () => {
    const { calls, call } = setup("3\n");
    const response = await call("GET", "/sessions/live?email=alice%40example.com");
    expect(await response.json()).toEqual({ live: 3 });
    expect(calls[0]?.variables).toEqual({ email: "alice@example.com" });
  });

  test("every statement runs as the schema owner and uses variables, not pasted values", () => {
    for (const sql of [SEED_SQL, ADD_MEMBERSHIP_SQL, CREATE_SESSION_SQL, LIVE_SESSIONS_SQL, createUserScript([{ tenant: "acme", role: "admin" }])]) {
      expect(sql.startsWith("SET ROLE identity_owner;")).toBe(true);
      expect(sql).not.toMatch(/example\.com|0000-4000/);
    }
  });
});

describe("the mail", () => {
  test("counts the mails to an address and gives the code in the latest", async () => {
    const mails: Mail[] = [
      { from: "n@example.com", to: ["alice@example.com"], text: "code 11111111" },
      { from: "n@example.com", to: ["bob@example.com"], text: "code 22222222" },
      { from: "n@example.com", to: ["Alice@Example.com"], text: "code 33333333" },
    ];
    const { call } = setup("x\n", mails);
    expect(await (await call("GET", "/mail?to=alice@example.com")).json()).toEqual({ count: 2, code: "33333333" });
    expect(await (await call("GET", "/mail?to=carol@example.com")).json()).toEqual({ count: 0, code: null });
  });
});
