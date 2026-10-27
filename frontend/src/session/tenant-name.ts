import type { I18n } from "../i18n/useI18n.ts";
import type { TenantMembership } from "./session.ts";

/** A tenant's name, or a translated stand-in for the rare tenant without one. */
export function tenantName(tenant: Pick<TenantMembership, "name">, t: I18n["t"]): string {
  return tenant.name ?? t("tenant_switcher.unnamed");
}

export const ROLE_LABELS = {
  viewer: "tenant_role.viewer",
  editor: "tenant_role.editor",
  admin: "tenant_role.admin",
} as const satisfies Record<TenantMembership["role"], string>;
