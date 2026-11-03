// The test's hand on the throwaway environment. The browser tests run in a container and can only
// reach HTTP, but a few things they need are not reachable through the product: users, tenant
// memberships and sessions are written by the schema owner (services/identity/README.md
// 「測試工具怎麼建立 session」, there is no API for them yet), and the sign-in code arrives in the
// mail relay. This is a small authenticated HTTP surface over those, on the loopback address of
// the machine that runs the environment; it exists only while a run does.
import { createHash, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";

import { cookieHashHex, newCookie } from "../lib/identity.ts";
import { codeIn, type Mail } from "./smtp.ts";

/**
 * The tenants every run starts with, by the key the tests use. Fixed ids: the run's database is
 * new, so nothing else holds them.
 */
export const TENANTS = {
  acme: { id: "00000000-0000-4000-8000-0000000945a1", name: "Acme Factory" },
  beta: { id: "00000000-0000-4000-8000-0000000945a2", name: "Beta Plant" },
} as const;
export type TenantKey = keyof typeof TENANTS;

export const ROLES = ["viewer", "editor", "admin"] as const;
export type Role = (typeof ROLES)[number];

/** Runs a psql script as the database's own superuser; `variables` are psql variables (`:'name'`). */
export interface Database {
  run(script: string, variables: Record<string, string>): Promise<string>;
}

/** Addresses the tests may create: lowercase (the service stores them so) and only on example.com. */
const EMAIL = /^[a-z0-9][a-z0-9._-]{0,40}@example\.com$/;

/** psql runs every statement as the schema owner, which is how the service's own tests write rows. */
const AS_OWNER = "SET ROLE identity_owner;\n";

export const SEED_SQL =
  AS_OWNER +
  "INSERT INTO identity.tenants (tenant_id, name) VALUES (:'acme_id', :'acme_name'), (:'beta_id', :'beta_name') " +
  "ON CONFLICT (tenant_id) DO UPDATE SET name = excluded.name;\n";

export function seedVariables(): Record<string, string> {
  return {
    acme_id: TENANTS.acme.id,
    acme_name: TENANTS.acme.name,
    beta_id: TENANTS.beta.id,
    beta_name: TENANTS.beta.name,
  };
}

/** A user and their memberships. Returns the user's id. */
export function createUserScript(memberships: readonly { tenant: TenantKey; role: Role }[]): string {
  let script = `${AS_OWNER}INSERT INTO identity.users (id, email, platform_admin) VALUES (:'user_id', :'email', false);\n`;
  memberships.forEach((_, index) => {
    script +=
      "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) " +
      `VALUES (:'tenant${index}', :'user_id', :'role${index}');\n`;
  });
  return script;
}

export const ADD_MEMBERSHIP_SQL =
  AS_OWNER +
  "INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) " +
  "SELECT :'tenant', u.id, :'role' FROM identity.users u WHERE u.email = :'email' " +
  "ON CONFLICT (tenant_id, user_id) DO UPDATE SET tenant_role = excluded.tenant_role RETURNING user_id;\n";

/** Only the SHA-256 of the cookie's 32 bytes reaches the database, as the service itself stores it. */
export const CREATE_SESSION_SQL =
  AS_OWNER +
  "INSERT INTO identity.sessions (id, token_hash, user_id, active_tenant_id, expires_at) " +
  "SELECT :'session_id', decode(:'hash', 'hex'), u.id, NULLIF(:'tenant', '')::uuid, now() + interval '1 day' " +
  "FROM identity.users u WHERE u.email = :'email' RETURNING id;\n";

export const LIVE_SESSIONS_SQL =
  AS_OWNER +
  "SELECT count(*) FROM identity.sessions s JOIN identity.users u ON u.id = s.user_id " +
  "WHERE u.email = :'email' AND s.revoked_at IS NULL AND s.expires_at > now();\n";

export interface ControlOptions {
  database: Database;
  mails(): readonly Mail[];
  /** The bearer token callers present. */
  token: string;
}

class Refused extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

function email(value: unknown): string {
  if (typeof value !== "string" || !EMAIL.test(value)) throw new Refused(422, "email must be a lowercase address on example.com");
  return value;
}

function tenantKey(value: unknown): TenantKey {
  if (value === "acme" || value === "beta") return value;
  throw new Refused(422, "tenant must be acme or beta");
}

function role(value: unknown): Role {
  const found = ROLES.find((candidate) => candidate === value);
  if (found === undefined) throw new Refused(422, "role must be viewer, editor or admin");
  return found;
}

function sameToken(given: string | null, expected: string): boolean {
  const left = createHash("sha256").update(given ?? "").digest();
  const right = createHash("sha256").update(expected).digest();
  return timingSafeEqual(left, right);
}

async function body(request: Request): Promise<Record<string, unknown>> {
  const parsed: unknown = await request.json().catch(() => null);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) throw new Refused(400, "a JSON object is required");
  return parsed as Record<string, unknown>;
}

/**
 * The request handler:
 * - `POST /users` `{email, memberships: [{tenant, role}]}` creates a user (a new id).
 * - `POST /memberships` `{email, tenant, role}` adds, or changes, a membership.
 * - `POST /sessions` `{email, tenant?}` creates a session and returns its cookie value.
 * - `GET /sessions/live?email=` counts the user's sessions that are not revoked or expired.
 * - `GET /mail?to=` returns how many messages the address has received and the code in the latest.
 */
export function createControl(options: ControlOptions): (request: Request) => Promise<Response> {
  const json = (value: unknown, status = 200) => Response.json(value, { status, headers: { "cache-control": "no-store" } });
  return async (request) => {
    try {
      if (!sameToken(request.headers.get("authorization")?.replace(/^Bearer /, "") ?? null, options.token)) {
        throw new Refused(401, "unauthorized");
      }
      const url = new URL(request.url);
      const route = `${request.method} ${url.pathname}`;
      if (route === "POST /users") {
        const input = await body(request);
        const list = input.memberships;
        if (!Array.isArray(list) || list.length > 4) throw new Refused(422, "memberships must be a list of at most 4");
        const memberships = list.map((item: unknown) => {
          const entry = (typeof item === "object" && item !== null ? item : {}) as Record<string, unknown>;
          return { tenant: tenantKey(entry.tenant), role: role(entry.role) };
        });
        const userId = randomUUID();
        const variables: Record<string, string> = { user_id: userId, email: email(input.email) };
        memberships.forEach((membership, index) => {
          variables[`tenant${index}`] = TENANTS[membership.tenant].id;
          variables[`role${index}`] = membership.role;
        });
        await options.database.run(createUserScript(memberships), variables);
        return json({ user_id: userId }, 201);
      }
      if (route === "POST /memberships") {
        const input = await body(request);
        const out = await options.database.run(ADD_MEMBERSHIP_SQL, {
          email: email(input.email),
          tenant: TENANTS[tenantKey(input.tenant)].id,
          role: role(input.role),
        });
        if (out.trim() === "") throw new Refused(404, "no such user");
        return json({ ok: true });
      }
      if (route === "POST /sessions") {
        const input = await body(request);
        const cookie = newCookie();
        const out = await options.database.run(CREATE_SESSION_SQL, {
          session_id: randomUUID(),
          hash: cookieHashHex(cookie),
          tenant: input.tenant === undefined ? "" : TENANTS[tenantKey(input.tenant)].id,
          email: email(input.email),
        });
        if (out.trim() === "") throw new Refused(404, "no such user");
        return json({ cookie }, 201);
      }
      if (route === "GET /sessions/live") {
        const out = await options.database.run(LIVE_SESSIONS_SQL, { email: email(url.searchParams.get("email")) });
        return json({ live: Number(out.trim()) });
      }
      if (route === "GET /mail") {
        const to = email(url.searchParams.get("to"));
        const received = options.mails().filter((mail) => mail.to.some((address) => address.toLowerCase() === to));
        const latest = received.at(-1);
        return json({ count: received.length, code: latest === undefined ? null : (codeIn(latest.text) ?? null) });
      }
      throw new Refused(404, "no such route");
    } catch (error) {
      if (error instanceof Refused) return json({ error: error.message }, error.status);
      return json({ error: "the control request failed" }, 500);
    }
  };
}

/** A random bearer token for one run. */
export function newToken(): string {
  return randomBytes(24).toString("base64url");
}
