import { LockIcon } from "lucide-react";

import { EmptyState } from "../../components/EmptyState.tsx";
import { useI18n } from "../../i18n/useI18n.ts";

/**
 * The tenant's plan does not include machines (Starter; docs/product/plans.md): what the feature
 * is for, and that it comes with Business and above. The design's "view plans" button is left out
 * because no plans page exists yet.
 */
export function PlanWall() {
  const { t } = useI18n();
  return (
    <EmptyState icon={<LockIcon aria-hidden="true" />} title={t("machines.plan.title")}>
      <p>{t("machines.plan.body")}</p>
      <p>{t("machines.plan.contact")}</p>
    </EmptyState>
  );
}
