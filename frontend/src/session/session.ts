import type { components } from "../api/schema.gen.ts";

export type MemberRole = components["schemas"]["MemberRole"];

/** A tenant the user is a member of, with their role in it. */
export interface TenantMembership {
  id: string;
  /** `null` only for a tenant without a record (data written outside the API); shown as unnamed. */
  name: string | null;
  role: MemberRole;
}

/** Who is signed in and in which tenant they are working. */
export interface SessionInfo {
  user: { id: string; email: string | null; platformAdmin: boolean };
  /** `null` when the session is in no tenant: never chosen, or no longer a member of it. */
  activeTenant: TenantMembership | null;
  /** Every tenant the user belongs to, by name. */
  tenants: readonly TenantMembership[];
}

/**
 * Where the SPA learns about the session and changes it. The identity service serves all of it
 * (`getCurrentSession`, `switchTenant`, `logOut`, `logOutEverywhere` in api/openapi.json);
 * `identityApi` (src/api/identity.ts) calls those endpoints, and tests pass their own.
 */
export interface SessionSource {
  /** The current session, or null when the browser has none (the server answered 401). */
  loadSession(signal?: AbortSignal): Promise<SessionInfo | null>;
  /** Makes `tenantId` the active tenant and returns the session after the switch. */
  switchTenant(tenantId: string): Promise<SessionInfo>;
  /** Ends this browser's session; resolves too when it had already ended. */
  logOut(): Promise<void>;
  /** Ends every session of the user, this one included. */
  logOutEverywhere(): Promise<void>;
}

/** The browser tab's title: the active tenant's name first, so tabs of two tenants differ. */
export function documentTitle(appName: string, tenantName: string | null | undefined): string {
  return tenantName ? `${tenantName} · ${appName}` : appName;
}
