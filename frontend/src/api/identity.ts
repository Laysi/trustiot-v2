import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import { fromApi, LANGUAGES, type Preferences, THEMES, toApiChange } from "../preferences/preferences.ts";
import type { SessionInfo, SessionSource, TenantMembership } from "../session/session.ts";
import { type ApiClient, pinnable, unwrap } from "./client.ts";
import { classifyError, ResponseShapeError } from "./errors.ts";
import type { components } from "./schema.gen.ts";

type Spec = components["schemas"];

// The identity service's bodies (services/identity/README.md), each bound to the generated type
// so a change in api/openapi.json fails `typecheck` (lib/type-binding.ts).
const MemberRoleSchema = z.enum(["viewer", "editor", "admin"]);
const TenantStateSchema = z.enum(["active", "suspended", "deleted"]);
const TenantMembershipSchema = z.object({
  tenant_id: z.string(),
  name: z.string().nullable(),
  role: MemberRoleSchema,
  state: TenantStateSchema,
});
const CurrentSessionSchema = z.object({
  user_id: z.string(),
  email: z.string().nullable(),
  platform_admin: z.boolean(),
  active_tenant: TenantMembershipSchema.nullable(),
  tenants: z.array(TenantMembershipSchema),
});
const PreferencesSchema = z.object({
  language: z.enum(LANGUAGES),
  ai_language: z.enum(LANGUAGES),
  time_zone: z.string(),
  theme: z.enum(THEMES),
});
const CodeSentSchema = z.object({
  code_length: z.number().int().positive(),
  expires_in_secs: z.number().nonnegative(),
  resend_after_secs: z.number().nonnegative(),
});
const LoggedInSchema = z.object({ redirect_to: z.string() });
/** `POST /auth/login` answers `send_code` with CodeSent and `verify_code` with LoggedIn. */
const LoginAnswerSchema = z.union([CodeSentSchema, LoggedInSchema]);

export type IdentityBodiesMatchSpec = Assert<
  AllTrue<
    [
      Equal<z.output<typeof TenantMembershipSchema>, Spec["TenantMembership"]>,
      Equal<z.output<typeof CurrentSessionSchema>, Spec["CurrentSession"]>,
      Equal<z.output<typeof PreferencesSchema>, Spec["Preferences"]>,
      Equal<z.output<typeof CodeSentSchema>, Spec["CodeSent"]>,
      Equal<z.output<typeof LoggedInSchema>, Spec["LoggedIn"]>,
      Equal<z.output<typeof LoginAnswerSchema>, Spec["CodeSent"] | Spec["LoggedIn"]>,
    ]
  >
>;

/** The answer to `send_code`: the same whether or not a user has the address. */
export interface CodeSent {
  codeLength: number;
  expiresInSecs: number;
  resendAfterSecs: number;
}

/** The email-code sign-in (services/identity/README.md 「登入」). */
export interface LoginApi {
  sendCode(email: string): Promise<CodeSent>;
  /** Resolves with the path to go to: the server's checked `redirect_to`. */
  verifyCode(input: { email: string; code: string; redirectTo: string }): Promise<{ redirectTo: string }>;
}

/** The user's own preferences (services/identity/README.md 「偏好設定」). */
export interface PreferencesApi {
  loadPreferences(signal?: AbortSignal): Promise<Preferences>;
  /** Saves the fields given and returns every preference as the server now has them. */
  updatePreferences(change: Partial<Preferences>): Promise<Preferences>;
}

export type IdentityApi = SessionSource & PreferencesApi & LoginApi;

function membership(body: z.output<typeof TenantMembershipSchema>): TenantMembership {
  return { id: body.tenant_id, name: body.name, role: body.role, state: body.state };
}

function sessionInfo(body: z.output<typeof CurrentSessionSchema>): SessionInfo {
  return {
    user: { id: body.user_id, email: body.email, platformAdmin: body.platform_admin },
    activeTenant: body.active_tenant ? membership(body.active_tenant) : null,
    tenants: body.tenants.map(membership),
  };
}

/**
 * The identity service's endpoints the SPA uses, through the typed client (`pinnable`: a page
 * binds its own copy).
 */
export function identityApi(client: ApiClient): IdentityApi {
  return pinnable(client, (client) => ({
    async loadSession(signal) {
      try {
        return sessionInfo(
          await unwrap(CurrentSessionSchema, client.GET("/api/identity/v1/sessions/current", signal ? { signal } : {})),
        );
      } catch (error) {
        // No session cookie, or one that has ended: signed out, which is not a failure here.
        if (classifyError(error) === "unauthenticated") return null;
        throw error;
      }
    },

    async switchTenant(tenantId) {
      return sessionInfo(
        await unwrap(
          CurrentSessionSchema,
          client.PUT("/api/identity/v1/sessions/current/tenant", { body: { tenant_id: tenantId } }),
        ),
      );
    },

    async logOut() {
      try {
        await unwrap(z.undefined(), client.DELETE("/api/identity/v1/sessions/current"));
      } catch (error) {
        // The session had already ended (revoked elsewhere, expired): there is nothing to end.
        if (classifyError(error) !== "unauthenticated") throw error;
      }
    },

    async logOutEverywhere() {
      // A 401 is not swallowed here: this session has ended, but the others have not, so the
      // caller tells the user to sign in and try again (session/useSignOut.ts).
      await unwrap(z.undefined(), client.DELETE("/api/identity/v1/sessions"));
    },

    async loadPreferences(signal) {
      return fromApi(
        await unwrap(PreferencesSchema, client.GET("/api/identity/v1/preferences", signal ? { signal } : {})),
      );
    },

    async updatePreferences(change) {
      return fromApi(
        await unwrap(PreferencesSchema, client.PATCH("/api/identity/v1/preferences", { body: toApiChange(change) })),
      );
    },

    async sendCode(email) {
      const pending = client.POST("/auth/login", { body: { step: "send_code", email } });
      const answer = await unwrap(LoginAnswerSchema, pending);
      if ("code_length" in answer) {
        return {
          codeLength: answer.code_length,
          expiresInSecs: answer.expires_in_secs,
          resendAfterSecs: answer.resend_after_secs,
        };
      }
      throw new ResponseShapeError((await pending).response.status, [{ path: "/code_length", code: "invalid_type" }]);
    },

    async verifyCode({ email, code, redirectTo }) {
      const pending = client.POST("/auth/login", {
        body: { step: "verify_code", email, code, redirect_to: redirectTo },
      });
      const answer = await unwrap(LoginAnswerSchema, pending);
      if ("redirect_to" in answer) {
        return { redirectTo: answer.redirect_to };
      }
      throw new ResponseShapeError((await pending).response.status, [{ path: "/redirect_to", code: "invalid_type" }]);
    },
  }));
}
