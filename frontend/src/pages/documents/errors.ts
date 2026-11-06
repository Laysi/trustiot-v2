import { ApiError } from "../../api/errors.ts";

/** Whether the library says the service is down rather than the request wrong. */
export function isUnavailable(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    (error.status >= 500 || error.body?.code === "unavailable" || error.body?.code === "timeout")
  );
}

/** Whether the tenant's plan has no document library: 403 `entitlement.feature_not_in_plan` (docs/product/plans.md). */
export function isNotInPlan(error: unknown): boolean {
  return (
    error instanceof ApiError && error.status === 403 && error.body?.message_key === "entitlement.feature_not_in_plan"
  );
}

/** Whether the document does not exist (never did, or the link is wrong). */
export function isNotFound(error: unknown): boolean {
  return error instanceof ApiError && error.status === 404;
}
