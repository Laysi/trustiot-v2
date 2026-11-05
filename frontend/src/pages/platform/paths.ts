/** The platform back office (docs/design/platform-admin/design.md), for platform administrators only. */
export const PLATFORM_PATH = "/platform";
export const PLATFORM_TENANTS_PATH = `${PLATFORM_PATH}/tenants`;
export const PLATFORM_TENANT_NEW_PATH = `${PLATFORM_TENANTS_PATH}/new`;
export const PLATFORM_GATEWAYS_PATH = `${PLATFORM_PATH}/gateways`;
export const PLATFORM_PROVISIONING_PATH = `${PLATFORM_PATH}/provisioning`;
export const PLATFORM_AUDIT_PATH = `${PLATFORM_PATH}/audit-log`;

/** The tabs of a tenant's page. */
export type TenantTab = "overview" | "members" | "gateways" | "audit";
export const TENANT_TABS: readonly TenantTab[] = ["overview", "members", "gateways", "audit"];

/** One tenant's page, on `tab` (the overview has no parameter). */
export function platformTenantPath(tenantId: string, tab: TenantTab = "overview"): string {
  const path = `${PLATFORM_TENANTS_PATH}/${encodeURIComponent(tenantId)}`;
  return tab === "overview" ? path : `${path}?tab=${tab}`;
}

/** The operation log, platform-wide or for one tenant (`?tenant=<ID>`). */
export function platformAuditPath(tenantId?: string): string {
  return tenantId === undefined ? PLATFORM_AUDIT_PATH : `${PLATFORM_AUDIT_PATH}?tenant=${encodeURIComponent(tenantId)}`;
}
