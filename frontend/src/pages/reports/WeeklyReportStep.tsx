import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";

import { useReportApi } from "../../api/pending/ReportApiProvider.tsx";
import { AskAiButton } from "../../components/ask/AskAiButton.tsx";
import { thisWeekUse } from "../../components/ask/questions.ts";
import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { isNotInPlan, useReportRole } from "./access.ts";
import { WEEKLY_REPORT_ONBOARDING_PATH } from "./paths.ts";
import { schedulesKey } from "./queries.ts";

/**
 * What the setup checklist's step "choose a first use" (Laysi/trustiot-v2#129) adds for reports
 * (docs/design/ms4-ms5/design.md 「設定清單的『設定每週報告』」): the main button leads to the weekly
 * report's form, filled in, next to the question for the AI. A plan without reports (Starter), a
 * viewer, or a plan that has not answered yet gets only the question: no upgrade wall in the
 * checklist. The checklist places it in the step; "skip this step" is the checklist's.
 */
export function WeeklyReportStep() {
  const { t } = useI18n();
  const api = useReportApi();
  const { canEdit, tenantId } = useReportRole();
  const schedules = useQuery({
    queryKey: schedulesKey(tenantId),
    queryFn: ({ signal }) => api.listSchedules(signal),
    enabled: canEdit,
  });
  const offered = canEdit && schedules.data !== undefined && !isNotInPlan(schedules.error);
  return (
    <div className="grid gap-3">
      {offered ? <p className="text-sm">{t("reports.onboarding.hint")}</p> : null}
      <div className="flex flex-wrap items-center gap-2">
        {offered ? (
          <Button asChild>
            <Link to={WEEKLY_REPORT_ONBOARDING_PATH}>{t("reports.onboarding.next")}</Link>
          </Button>
        ) : null}
        <AskAiButton {...thisWeekUse(t)} />
      </div>
    </div>
  );
}
