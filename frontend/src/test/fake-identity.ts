import { ApiError, type ErrorBody, type ErrorCode } from "../api/errors.ts";
import type { IdentityApi } from "../api/identity.ts";
import { DEFAULT_PREFERENCES, isTimeZone, LANGUAGES, type Preferences, THEMES } from "../preferences/preferences.ts";
import type { MemberRole, SessionInfo, TenantMembership } from "../session/session.ts";

/**
 * An identity service held in memory, for the component tests: users, tenants, server-side
 * sessions, preferences and the email-code sign-in, with the answers services/identity/README.md
 * documents (401 without a session, 403 `session.tenant_not_member`, 422 with field errors).
 * Each `browser()` keeps its own cookie, so a test can sign in on two browsers and watch one
 * sign-out end both, or see a preference saved in one appear in the other.
 */

export type Operation = keyof IdentityApi;

/** An M1 error as the API client throws it. */
export function apiError(
  status: number,
  code: ErrorCode,
  messageKey: string,
  fieldErrors?: { field: string; message_key: string; details?: Record<string, string | number> }[],
): ApiError {
  const body: ErrorBody = { code, message_key: messageKey };
  if (fieldErrors) body.field_errors = fieldErrors;
  return new ApiError(status, body);
}

export const unauthenticated = () => apiError(401, "unauthenticated", "error.unauthenticated");
export const unavailable = () => apiError(503, "unavailable", "error.unavailable");
export const tenantSuspended = () => apiError(403, "tenant_suspended", "error.tenant_suspended");
export const validationFailed = (field: string, messageKey: string) =>
  apiError(422, "validation_failed", "error.validation_failed", [{ field, message_key: messageKey }]);

export interface FakeTenant {
  id: string;
  name: string | null;
}

export interface FakeUser {
  id: string;
  email: string;
  platformAdmin: boolean;
  /** Tenant ID to role, in the order added. */
  memberships: Map<string, MemberRole>;
  preferences: Preferences;
}

interface FakeSession {
  userId: string;
  activeTenant: string | null;
  ended: boolean;
}

/** One browser: its own cookie jar against the shared server. */
export type FakeBrowser = IdentityApi & {
  /** The session cookie this browser holds, if any. */
  readonly cookie: string | null;
};

const CODE_LENGTH = 8;

export class FakeIdentity {
  readonly tenants = new Map<string, FakeTenant>();
  /** By email address. */
  readonly users = new Map<string, FakeUser>();
  readonly sessions = new Map<string, FakeSession>();
  /** Every code sent, newest last. */
  readonly outbox: { email: string; code: string }[] = [];
  /** Every request, in order, with its input. */
  readonly requests: { operation: Operation; input?: unknown }[] = [];
  /** The wait `send_code` asks for before another code (the server's is 60 seconds). */
  resendAfterSecs = 60;
  /** The codes that can still be used, by address. */
  private readonly codes = new Map<string, string>();
  private readonly failures = new Map<Operation, unknown[]>();
  private readonly holds = new Map<Operation, Promise<void>[]>();
  private readonly answerHolds = new Map<Operation, Promise<void>[]>();
  private ids = 0;

  private nextId(prefix: string): string {
    this.ids += 1;
    return `${prefix}-${this.ids}`;
  }

  addTenant(name: string | null): FakeTenant {
    const tenant = { id: this.nextId("tenant"), name };
    this.tenants.set(tenant.id, tenant);
    return tenant;
  }

  addUser(
    email: string,
    options: {
      platformAdmin?: boolean;
      memberships?: readonly (readonly [FakeTenant, MemberRole])[];
      preferences?: Partial<Preferences>;
    } = {},
  ): FakeUser {
    const user: FakeUser = {
      id: this.nextId("user"),
      email,
      platformAdmin: options.platformAdmin ?? false,
      memberships: new Map((options.memberships ?? []).map(([tenant, role]) => [tenant.id, role])),
      preferences: { ...DEFAULT_PREFERENCES, ...options.preferences },
    };
    this.users.set(email, user);
    return user;
  }

  /** The next `operation` throws `error` instead of answering (once per call). */
  failNext(operation: Operation, error: unknown): void {
    const queue = this.failures.get(operation) ?? [];
    queue.push(error);
    this.failures.set(operation, queue);
  }

  /** The next `operation` waits for `until` before it answers. */
  holdNext(operation: Operation, until: Promise<void>): void {
    const queue = this.holds.get(operation) ?? [];
    queue.push(until);
    this.holds.set(operation, queue);
  }

  /**
   * The next `operation` is carried out at once, but its answer waits for `until`: the server has
   * already changed its state when the answer is still on its way.
   */
  holdNextAnswer(operation: Operation, until: Promise<void>): void {
    const queue = this.answerHolds.get(operation) ?? [];
    queue.push(until);
    this.answerHolds.set(operation, queue);
  }

  /** A session made directly, as `bun scripts/dev.ts seed` does; returns its cookie. */
  createSession(email: string, activeTenant: FakeTenant | null = null): string {
    const user = this.users.get(email);
    if (!user) throw new Error(`no fake user ${email}`);
    const cookie = this.nextId("cookie");
    this.sessions.set(cookie, { userId: user.id, activeTenant: activeTenant?.id ?? null, ended: false });
    return cookie;
  }

  /** A browser that holds `cookie` (none: signed out). */
  browser(cookie: string | null = null): FakeBrowser {
    let jar = cookie;

    const answer = async <T>(operation: Operation, input: unknown, respond: () => T): Promise<T> => {
      this.requests.push(input === undefined ? { operation } : { operation, input });
      const hold = this.holds.get(operation)?.shift();
      if (hold) await hold;
      const failure = this.failures.get(operation)?.shift();
      if (failure !== undefined) throw failure;
      const answerHold = this.answerHolds.get(operation)?.shift();
      try {
        return respond();
      } finally {
        if (answerHold) await answerHold;
      }
    };

    const current = (): { session: FakeSession; user: FakeUser } => {
      const session = jar === null ? undefined : this.sessions.get(jar);
      const user = session && !session.ended ? this.userById(session.userId) : undefined;
      if (!session || !user) throw unauthenticated();
      return { session, user };
    };

    const browser: FakeBrowser = {
      get cookie() {
        return jar;
      },

      loadSession: () =>
        answer("loadSession", undefined, () => {
          try {
            const { session, user } = current();
            return this.sessionInfo(session, user);
          } catch {
            return null;
          }
        }),

      switchTenant: (tenantId) =>
        answer("switchTenant", tenantId, () => {
          const { session, user } = current();
          if (!user.memberships.has(tenantId)) {
            throw apiError(403, "permission_denied", "session.tenant_not_member");
          }
          session.activeTenant = tenantId;
          return this.sessionInfo(session, user);
        }),

      logOut: () =>
        answer("logOut", undefined, () => {
          const session = jar === null ? undefined : this.sessions.get(jar);
          if (session) session.ended = true;
          jar = null;
        }),

      logOutEverywhere: () =>
        answer("logOutEverywhere", undefined, () => {
          const { user } = current();
          for (const session of this.sessions.values()) {
            if (session.userId === user.id) session.ended = true;
          }
          jar = null;
        }),

      loadPreferences: () => answer("loadPreferences", undefined, () => ({ ...current().user.preferences })),

      updatePreferences: (change) =>
        answer("updatePreferences", change, () => {
          const { user } = current();
          const refused = this.refusedPreference(change);
          if (refused) throw refused;
          user.preferences = { ...user.preferences, ...change };
          return { ...user.preferences };
        }),

      sendCode: (email) =>
        answer("sendCode", email, () => {
          if (!/^[^@\s]+@[^@\s]+$/.test(email)) throw validationFailed("/email", "login.email_invalid");
          // The same answer whether or not the address has an account; only accounts get a code.
          if (this.users.has(email)) {
            const code = String(10_000_000 + this.outbox.length).slice(0, CODE_LENGTH);
            this.codes.set(email, code);
            this.outbox.push({ email, code });
          }
          return { codeLength: CODE_LENGTH, expiresInSecs: 600, resendAfterSecs: this.resendAfterSecs };
        }),

      verifyCode: (input) =>
        answer("verifyCode", input, () => {
          if (!/^\d{8}$/.test(input.code)) throw validationFailed("/code", "login.code_invalid");
          const user = this.users.get(input.email);
          if (!user || this.codes.get(input.email) !== input.code) throw unauthenticated();
          this.codes.delete(input.email);
          // Like the server: the browser's old session ends, and the new one keeps the tenant of
          // the user's latest session that had one.
          const previous = jar === null ? undefined : this.sessions.get(jar);
          if (previous) previous.ended = true;
          const latest = [...this.sessions.values()].reverse().find((s) => s.userId === user.id && s.activeTenant);
          jar = this.nextId("cookie");
          this.sessions.set(jar, { userId: user.id, activeTenant: latest?.activeTenant ?? null, ended: false });
          return { redirectTo: input.redirectTo };
        }),
    };
    return browser;
  }

  /** The operations requested so far, in order. */
  operations(): Operation[] {
    return this.requests.map((request) => request.operation);
  }

  private userById(id: string): FakeUser | undefined {
    for (const user of this.users.values()) {
      if (user.id === id) return user;
    }
    return undefined;
  }

  private membership(user: FakeUser, tenantId: string): TenantMembership | null {
    const role = user.memberships.get(tenantId);
    const tenant = this.tenants.get(tenantId);
    return role && tenant ? { id: tenant.id, name: tenant.name, role } : null;
  }

  private sessionInfo(session: FakeSession, user: FakeUser): SessionInfo {
    const tenants = [...user.memberships.keys()]
      .map((id) => this.membership(user, id))
      .filter((membership) => membership !== null)
      .sort((a, b) => (a.name ?? "").localeCompare(b.name ?? ""));
    return {
      user: { id: user.id, email: user.email, platformAdmin: user.platformAdmin },
      activeTenant: session.activeTenant === null ? null : this.membership(user, session.activeTenant),
      tenants,
    };
  }

  /** The 422 the server answers for a value it does not accept, every bad field listed. */
  private refusedPreference(change: Partial<Preferences>): ApiError | null {
    const fields: { field: string; message_key: string }[] = [];
    const isLanguage = (value: string) => (LANGUAGES as readonly string[]).includes(value);
    if (change.language !== undefined && !isLanguage(change.language)) {
      fields.push({ field: "/language", message_key: "preferences.language_invalid" });
    }
    if (change.aiLanguage !== undefined && !isLanguage(change.aiLanguage)) {
      fields.push({ field: "/ai_language", message_key: "preferences.ai_language_invalid" });
    }
    if (change.timeZone !== undefined && change.timeZone !== "auto" && !isTimeZone(change.timeZone)) {
      fields.push({ field: "/time_zone", message_key: "preferences.time_zone_invalid" });
    }
    if (change.theme !== undefined && !(THEMES as readonly string[]).includes(change.theme)) {
      fields.push({ field: "/theme", message_key: "preferences.theme_invalid" });
    }
    return fields.length > 0 ? apiError(422, "validation_failed", "error.validation_failed", fields) : null;
  }
}
