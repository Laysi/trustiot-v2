import { InfoIcon } from "lucide-react";

import type { Plan } from "../../api/gateways.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";

const PLAN_NAMES: Readonly<Record<Plan, MessageId>> = {
  starter: "gateways.plan.starter",
  business: "gateways.plan.business",
  enterprise: "gateways.plan.enterprise",
  enterprise_on_prem: "gateways.plan.enterprise_on_prem",
};

/** The plan that follows `plan` and what it allows, or `null` for the top one (docs/product/plans.md). */
const NEXT_PLAN: Readonly<Record<Plan, { plan: Plan; allowance: MessageId } | null>> = {
  starter: { plan: "business", allowance: "gateways.quota.allowance_business" },
  business: { plan: "enterprise", allowance: "gateways.quota.allowance_enterprise" },
  enterprise: null,
  enterprise_on_prem: null,
};

/**
 * The plan's gateway limit is reached (design.md, 「UpgradeNotice」): what is full, that what
 * exists keeps working, and what the next plan gives. Until there is billing, upgrading means
 * contacting us. Decision 8's link to the plan and usage section of the settings page is not here
 * because that section does not exist yet.
 * `id` is what the disabled bind button points to with `aria-describedby`.
 */
export function GatewayQuotaNotice({ id, plan, used, limit }: { id: string; plan: Plan; used: number; limit: number }) {
  const { t } = useI18n();
  const next = NEXT_PLAN[plan];
  return (
    <Alert id={id} role="status">
      <InfoIcon aria-hidden="true" />
      <AlertTitle>{t("gateways.quota.title", { plan: t(PLAN_NAMES[plan]), used, limit })}</AlertTitle>
      <AlertDescription>
        <p>
          {next
            ? t("gateways.quota.body_upgrade", { plan: t(PLAN_NAMES[next.plan]), allowance: t(next.allowance) })
            : t("gateways.quota.body_top")}
        </p>
      </AlertDescription>
    </Alert>
  );
}

export { PLAN_NAMES };
