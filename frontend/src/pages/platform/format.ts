import type { Limit, Plan, SuspensionReason, TenantStatus } from "../../api/pending/platform.ts";
import type { Locale, MessageId } from "../../i18n/messages.ts";

/** US dollars, as the AI allowance is counted. */
export function formatUsd(value: number, locale: Locale): string {
  return new Intl.NumberFormat(locale, { style: "currency", currency: "USD" }).format(value);
}

/** The reasons' texts, by the API's value. */
export const REASON_LABELS: Record<SuspensionReason, MessageId> = {
  non_payment: "platform.reason.non_payment",
  security_or_abuse: "platform.reason.security_or_abuse",
  contract_terminated: "platform.reason.contract_terminated",
};

/** A suspension that cannot be undone: the contract was terminated (`tenant.state_final`). */
export function isFinal(tenant: { state: string; reason: SuspensionReason | null }): boolean {
  return tenant.state === "deleted" || tenant.reason === "contract_terminated";
}

export function limitNumber(limit: Limit): number {
  return limit === "unlimited" ? Number.POSITIVE_INFINITY : limit;
}

/** Whether a used count has reached its limit. */
export function isFull(used: number, limit: Limit): boolean {
  return limit !== "unlimited" && used >= limit;
}

/** A status that is a suspension in progress or done. */
export function isSuspension(status: TenantStatus): boolean {
  return status === "processing" || status === "suspended" || status === "deleted";
}

export const PLAN_LABELS: Record<Plan, MessageId> = {
  starter: "platform.plan.starter",
  business: "platform.plan.business",
  enterprise: "platform.plan.enterprise",
  enterprise_on_prem: "platform.plan.enterprise_on_prem",
};
