import { ApiError } from "../../api/errors.ts";
import { useSession } from "../../session/SessionProvider.tsx";

/** The message key of a refusal because the plan lacks the feature (`crates/contract/src/entitlements.rs`). */
const FEATURE_NOT_IN_PLAN = "entitlement.feature_not_in_plan";

/** Whether `error` says the tenant's plan has no reports (Starter): the page shows the upgrade wall. */
export function isNotInPlan(error: unknown): boolean {
  return error instanceof ApiError && error.body?.message_key === FEATURE_NOT_IN_PLAN;
}

/**
 * What the signed-in member may do on the reports pages (design 「角色」): every member reads and
 * downloads, an editor or admin sets up schedules, and only an admin can upgrade the plan. The
 * role is the session's, never the fake's.
 */
export function useReportRole(): { canEdit: boolean; isAdmin: boolean; tenantId: string; tenantName: string } {
  const tenant = useSession().session?.activeTenant;
  const role = tenant?.role;
  return {
    canEdit: role === "editor" || role === "admin",
    isAdmin: role === "admin",
    tenantId: tenant?.id ?? "",
    tenantName: tenant?.name ?? "",
  };
}
