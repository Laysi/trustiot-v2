import { PauseCircleIcon, PlayIcon, SparklesIcon } from "lucide-react";

import type { AlertsOverview } from "../../api/pending/alerts.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { ConfirmAction } from "../dashboards/ConfirmAction.tsx";
import { usePauseAiRules, useResumeAiRules } from "./actions.ts";
import { useAlertsTime } from "./time.ts";

/**
 * The AI rules' state above both tabs (design 「AI 規則狀態條」, guardrail 5 of alert-rules.md): the
 * running count and the one-click pause of every AI-made rule while they run, and while they are
 * paused a bordered notice with who and when, which says what keeps running, and the button that
 * resumes them. Resuming needs no confirmation (a wrong resume is cheap, the pause is the safe state).
 */
export function AiRuleStrip({
  overview,
  canManage,
  onError,
}: {
  overview: AlertsOverview;
  canManage: boolean;
  onError(error: unknown): void;
}) {
  const { t, formatNumber } = useI18n();
  const time = useAlertsTime();
  const pause = usePauseAiRules();
  const resume = useResumeAiRules();

  if (overview.pause.paused) {
    return (
      <Alert role="status" className="grid-cols-[1.25rem_1fr] border-2 border-foreground">
        <PauseCircleIcon aria-hidden="true" className="size-5" />
        <AlertTitle className="text-base">{t("alerts.paused.title")}</AlertTitle>
        <AlertDescription className="text-foreground">
          <p>
            {overview.pause.by !== undefined && overview.pause.at !== undefined
              ? t("alerts.paused.by", { name: overview.pause.by, time: time.full(overview.pause.at) })
              : t("alerts.paused.unknown_by")}{" "}
            {t("alerts.paused.effect", {
              ai: formatNumber(overview.ai_rule_count),
              member: formatNumber(overview.member_rule_count),
            })}
          </p>
          {canManage ? (
            <div className="mt-2">
              <Button size="sm" disabled={resume.isPending} onClick={() => resume.mutate(undefined, { onError })}>
                {resume.isPending ? <Spinner /> : <PlayIcon aria-hidden="true" />}
                {t("alerts.paused.resume")}
              </Button>
            </div>
          ) : (
            <p className="text-muted-foreground">{t("alerts.needs_editor")}</p>
          )}
        </AlertDescription>
      </Alert>
    );
  }

  if (overview.ai_rule_count === 0) return null;
  return (
    <section
      aria-label={t("alerts.strip.label")}
      className="flex flex-wrap items-center gap-x-4 gap-y-2 rounded-lg bg-brand-soft px-4 py-3 text-sm text-foreground"
    >
      <SparklesIcon aria-hidden="true" className="size-4 shrink-0 text-primary" />
      <p className="min-w-0 flex-[1_1_20rem]">
        <b>{t("alerts.strip.running", { count: formatNumber(overview.ai_rule_count) })}</b>{" "}
        {canManage ? t("alerts.strip.explain") : t("alerts.strip.explain_viewer")}
      </p>
      {canManage ? (
        <ConfirmAction
          trigger={
            <Button variant="outline" size="sm" className="bg-card">
              <PauseCircleIcon aria-hidden="true" />
              {t("alerts.pause.action")}
            </Button>
          }
          title={t("alerts.pause.title")}
          body={t("alerts.pause.body", {
            ai: formatNumber(overview.ai_rule_count),
            member: formatNumber(overview.member_rule_count),
          })}
          confirm={t("alerts.pause.action")}
          pending={t("alerts.pause.pending")}
          mutation={pause}
        />
      ) : null}
    </section>
  );
}
