import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ClockIcon, TriangleAlertIcon, UsersIcon } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";

import { useReportApi } from "../../api/pending/ReportApiProvider.tsx";
import type { ReportSchedule } from "../../api/pending/reports.ts";
import { RowMenu } from "../../components/RowMenu.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { Card } from "../../components/ui/card.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { KIND_NAME, periodText } from "./kinds.ts";
import { editSchedulePath } from "./paths.ts";
import { reportListsKey, schedulesKey } from "./queries.ts";
import { weekdayName } from "./weekday.ts";

export interface Notice {
  kind: "ok" | "error";
  text: string;
}

/** A schedule as a card (docs/design/ms4-ms5/reports.html): its switch, facts, last and next run, and actions. */
export function ScheduleCard({
  schedule,
  canEdit,
  tenantId,
  onNotice,
}: {
  schedule: ReportSchedule;
  canEdit: boolean;
  tenantId: string;
  onNotice(notice: Notice): void;
}) {
  const { t, locale, errorMessage, formatDateTime } = useI18n();
  const api = useReportApi();
  const queryClient = useQueryClient();
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const name = t(KIND_NAME[schedule.kind]);
  const refresh = () =>
    Promise.all([
      queryClient.invalidateQueries({ queryKey: schedulesKey(tenantId) }),
      queryClient.invalidateQueries({ queryKey: reportListsKey(tenantId) }),
    ]);

  const toggle = useMutation({
    mutationFn: (enabled: boolean) => api.setScheduleEnabled(schedule.id, enabled),
    onSuccess: async (updated) => {
      await refresh();
      onNotice({
        kind: "ok",
        text: t(updated.enabled ? "reports.message.enabled" : "reports.message.disabled", { name }),
      });
    },
    onError: (error) => onNotice({ kind: "error", text: errorMessage(error) }),
  });
  const generate = useMutation({
    mutationFn: () => api.generateNow(schedule.id),
    onSuccess: async (report) => {
      await refresh();
      onNotice({
        kind: "ok",
        text: t("reports.message.generating", {
          name,
          period: periodText(report.period_start, report.period_end, new Date().getFullYear()),
        }),
      });
    },
    onError: (error) => onNotice({ kind: "error", text: errorMessage(error) }),
  });
  const remove = useMutation({
    mutationFn: () => api.deleteSchedule(schedule.id),
    onSuccess: async () => {
      setConfirmingDelete(false);
      await refresh();
      onNotice({ kind: "ok", text: t("reports.message.deleted", { name }) });
    },
  });

  const scopeText =
    schedule.scope.kind === "all"
      ? t("reports.schedules.scope_all")
      : t("reports.schedules.scope_devices", { count: schedule.scope.device_ids.length });
  const timing =
    schedule.kind === "weekly_energy"
      ? t("reports.schedules.timing_weekly", { weekday: weekdayName(schedule.send_on, locale), time: schedule.time })
      : t("reports.schedules.timing_monthly", { day: String(schedule.send_on), time: schedule.time });
  const run = schedule.last_run;
  const headingId = `schedule-${schedule.id}`;

  return (
    <Card aria-labelledby={headingId} className="content-start gap-3 px-4 py-4">
      <div className="flex items-start justify-between gap-3">
        <div className="grid min-w-0 gap-0.5">
          <h3 id={headingId} className="font-semibold">
            {name}
          </h3>
          <p className="text-sm text-muted-foreground">
            {scopeText} ·{" "}
            {t(
              schedule.kind === "weekly_energy"
                ? "reports.schedules.period_weekly"
                : "reports.schedules.period_monthly",
            )}
          </p>
        </div>
        {canEdit ? (
          <button
            type="button"
            role="switch"
            aria-checked={schedule.enabled}
            aria-label={t("reports.schedules.switch_label", { name })}
            disabled={toggle.isPending}
            onClick={() => toggle.mutate(!schedule.enabled)}
            className={cn(
              "relative h-6 w-11 shrink-0 rounded-full border border-input transition-colors",
              schedule.enabled ? "bg-primary" : "bg-muted",
              focusRing,
            )}
          >
            <span
              aria-hidden="true"
              className={cn(
                "absolute top-0.5 left-0.5 size-4.5 rounded-full bg-background shadow-sm transition-transform",
                schedule.enabled && "translate-x-5",
              )}
            />
          </button>
        ) : (
          <span className="rounded-full border bg-muted px-2 py-0.5 text-xs font-medium whitespace-nowrap">
            {t(schedule.enabled ? "reports.schedules.state_on" : "reports.schedules.state_off")}
          </span>
        )}
      </div>

      <ul className="flex flex-wrap gap-x-4 gap-y-1 text-sm text-muted-foreground">
        <li className="inline-flex items-center gap-1.5">
          <ClockIcon aria-hidden="true" className="size-4" />
          {timing}
        </li>
        <li className="inline-flex items-center gap-1.5">
          <UsersIcon aria-hidden="true" className="size-4" />
          {t("reports.schedules.recipients", { count: schedule.recipients.length })}
        </li>
      </ul>

      <p className="text-sm">
        {schedule.next_run_at ? (
          <>
            {t("reports.schedules.next")}{" "}
            <time dateTime={schedule.next_run_at}>{formatDateTime(schedule.next_run_at)}</time>
          </>
        ) : (
          t("reports.schedules.paused")
        )}
        {" · "}
        {run ? (
          <>
            {t(run.outcome === "sent" ? "reports.schedules.last_sent" : "reports.schedules.last_failed")}{" "}
            <time dateTime={run.at}>{formatDateTime(run.at)}</time>
          </>
        ) : (
          t("reports.schedules.never")
        )}
      </p>

      {schedule.bounced_recipients.length > 0 ? (
        <Alert variant="destructive" className="py-2">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertDescription>
            <p>{t("reports.schedules.bounced", { recipients: schedule.bounced_recipients.join(", ") })}</p>
            {canEdit ? (
              <Link
                to={editSchedulePath(schedule.id)}
                className={cn("rounded-sm text-sm font-medium text-foreground underline underline-offset-4", focusRing)}
              >
                {t("reports.schedules.change_recipients")}
              </Link>
            ) : null}
          </AlertDescription>
        </Alert>
      ) : null}

      {canEdit ? (
        <div className="flex flex-wrap items-center gap-2">
          <Button asChild variant="outline" size="sm">
            <Link to={editSchedulePath(schedule.id)} aria-label={t("reports.schedules.edit_named", { name })}>
              {t("reports.schedules.edit")}
            </Link>
          </Button>
          <Button
            variant="ghost"
            size="sm"
            disabled={generate.isPending}
            aria-label={t("reports.schedules.generate_named", { name })}
            onClick={() => generate.mutate()}
          >
            {generate.isPending ? <Spinner /> : null}
            {t("reports.schedules.generate")}
          </Button>
          <RowMenu
            label={t("reports.schedules.more_named", { name })}
            items={[
              {
                kind: "action",
                label: t("reports.schedules.delete"),
                destructive: true,
                onSelect: () => setConfirmingDelete(true),
              },
            ]}
          />
        </div>
      ) : null}

      <AlertDialog
        open={confirmingDelete}
        onOpenChange={(open) => {
          if (remove.isPending) return;
          if (open) remove.reset();
          setConfirmingDelete(open);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("reports.schedules.delete_title", { name })}</AlertDialogTitle>
            <AlertDialogDescription>{t("reports.schedules.delete_body")}</AlertDialogDescription>
          </AlertDialogHeader>
          {remove.isError ? (
            <Alert variant="destructive">
              <AlertDescription>
                <p>{errorMessage(remove.error)}</p>
              </AlertDescription>
            </Alert>
          ) : null}
          <AlertDialogFooter>
            <AlertDialogCancel disabled={remove.isPending}>{t("reports.cancel")}</AlertDialogCancel>
            <Button variant="destructive" disabled={remove.isPending} onClick={() => remove.mutate()}>
              {remove.isPending ? <Spinner /> : null}
              {t(remove.isPending ? "reports.schedules.delete_pending" : "reports.schedules.delete_confirm")}
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Card>
  );
}
