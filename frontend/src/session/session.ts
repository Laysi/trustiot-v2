/** A tenant the user is a member of, with their role in it. */
export interface TenantMembership {
  id: string;
  name: string;
  role: string;
}

/** Who is signed in and in which tenant they are working. */
export interface SessionInfo {
  user: { id: string; displayName: string };
  activeTenant: { id: string; name: string } | null;
  tenants: readonly TenantMembership[];
}

/**
 * Where the SPA learns about the session and switches tenants. The identity service has no
 * session or tenant-switch endpoint yet (api/openapi.json); the pages are written against this
 * interface and the real source plugs in when the endpoint exists.
 */
export interface SessionSource {
  /** The current session, or null when there is none to show. */
  load(signal?: AbortSignal): Promise<SessionInfo | null>;
  /** Makes `tenantId` the active tenant and returns the session after the switch. */
  switchTenant(tenantId: string): Promise<SessionInfo>;
}

/** Until the identity service has a session endpoint: no session to show, no switching. */
export const noSessionSource: SessionSource = {
  load: async () => null,
  switchTenant: async () => {
    throw new Error("switching tenants needs the identity service's session endpoint");
  },
};

/** The browser tab's title: the active tenant's name first, so tabs of two tenants differ. */
export function documentTitle(appName: string, tenantName: string | undefined): string {
  return tenantName ? `${tenantName} · ${appName}` : appName;
}
