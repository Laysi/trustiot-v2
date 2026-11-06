import { FileTextIcon } from "lucide-react";

import { useI18n } from "../../i18n/useI18n.ts";
import { AskAiButton } from "./AskAiButton.tsx";
import { useReportRole } from "./access.ts";

/**
 * What a tenant without the reports entitlement (Starter) sees: the upgrade wall of
 * docs/design/ms4-ms5/reports.html, with what it can do now (ask the AI, export CSV from a device's page).
 */
export function PlanWall() {
  const { t } = useI18n();
  const { isAdmin } = useReportRole();
  return (
    <div role="status" className="grid justify-items-center gap-3 rounded-lg border px-4 py-12 text-center">
      <FileTextIcon aria-hidden="true" className="size-6 text-muted-foreground" />
      <h2 className="text-lg font-semibold">{t("reports.wall.title")}</h2>
      <p className="max-w-xl text-sm text-muted-foreground">{t("reports.wall.body")}</p>
      <p className="text-sm font-medium">{t(isAdmin ? "reports.wall.admin" : "reports.wall.member")}</p>
      <AskAiButton question={t("reports.ask.last_month")} />
    </div>
  );
}
