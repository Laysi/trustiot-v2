import { LockIcon } from "lucide-react";

import { EmptyState } from "../../components/EmptyState.tsx";
import { useI18n } from "../../i18n/useI18n.ts";

/** What a tenant sees where its plan has no document library (Business and above, docs/product/plans.md). */
export function NotInPlan() {
  const { t } = useI18n();
  return (
    <EmptyState centered icon={<LockIcon aria-hidden="true" />} title={t("documents.starter.title")}>
      <p>{t("documents.starter.body")}</p>
      <p>{t("entitlement.feature_not_in_plan")}</p>
    </EmptyState>
  );
}
