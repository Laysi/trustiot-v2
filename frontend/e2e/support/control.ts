// The client of the control surface that `scripts/browser-e2e/control.ts` serves next to the
// environment: users and sessions, which no API creates yet, and the sign-in code from the mail
// relay. The tests run in a container; this is their only way to what is behind the product.
import { randomBytes } from "node:crypto";

import { expect } from "@playwright/test";

export type TenantKey = "acme" | "beta";
export type Role = "viewer" | "editor" | "admin";

/** The names the run's two tenants carry, as the pages show them. */
export const TENANT_NAMES: Record<TenantKey, string> = { acme: "Acme Factory", beta: "Beta Plant" };

/** The session cookie's name (services/identity/README.md 「session」). */
export const SESSION_COOKIE = "__Host-trustiot-session";

function required(name: string): string {
  const value = process.env[name];
  if (value === undefined || value === "") throw new Error(`${name} is not set (see frontend/README.md)`);
  return value;
}

/** A new address on example.com that no other test uses, so every test has its own sign-in limits. */
export function uniqueEmail(prefix: string): string {
  return `${prefix}-${randomBytes(4).toString("hex")}@example.com`;
}

export class Control {
  constructor(
    private readonly origin: string,
    private readonly token: string,
  ) {}

  private async call(method: "GET" | "POST", path: string, body?: unknown): Promise<unknown> {
    const response = await fetch(`${this.origin}${path}`, {
      method,
      headers: {
        authorization: `Bearer ${this.token}`,
        ...(body === undefined ? {} : { "content-type": "application/json" }),
      },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const text = await response.text();
    if (!response.ok) throw new Error(`control ${method} ${path} answered ${response.status}: ${text}`);
    return JSON.parse(text);
  }

  async createUser(email: string, memberships: readonly { tenant: TenantKey; role: Role }[]): Promise<void> {
    await this.call("POST", "/users", { email, memberships });
  }

  async addMembership(email: string, tenant: TenantKey, role: Role): Promise<void> {
    await this.call("POST", "/memberships", { email, tenant, role });
  }

  /** A session of the user's, without the email sign-in: its cookie value, and the tenant it starts in (none by default). */
  async createSession(email: string, tenant?: TenantKey): Promise<string> {
    const answer = (await this.call("POST", "/sessions", { email, ...(tenant === undefined ? {} : { tenant }) })) as {
      cookie: string;
    };
    return answer.cookie;
  }

  /** How many sessions of the user are neither revoked nor expired, read from the database. */
  async liveSessions(email: string): Promise<number> {
    const answer = (await this.call("GET", `/sessions/live?email=${encodeURIComponent(email)}`)) as { live: number };
    return answer.live;
  }

  /** The code in the latest mail to the address, once there is one. */
  async waitForCode(email: string): Promise<string> {
    const found: { code: string | null } = { code: null };
    await expect
      .poll(
        async () => {
          const answer = (await this.call("GET", `/mail?to=${encodeURIComponent(email)}`)) as { code: string | null };
          found.code = answer.code;
          return found.code;
        },
        { message: `the sign-in code mailed to ${email}`, timeout: 60_000, intervals: [250, 500, 1000] },
      )
      .not.toBeNull();
    if (found.code === null) throw new Error(`no sign-in code for ${email}`);
    return found.code;
  }
}

export function controlFromEnvironment(): Control {
  return new Control(required("E2E_CONTROL_URL"), required("E2E_CONTROL_TOKEN"));
}
