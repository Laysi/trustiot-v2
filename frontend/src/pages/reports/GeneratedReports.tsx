import { useMutation, useQueryClient } from "@tanstack/react-query";
import { DownloadIcon, RotateCcwIcon } from "lucide-react";
import { Link } from "react-router";

import { useReportApi } from "../../api/pending/ReportApiProvider.tsx";
import type { DownloadFormat, ReportSummary } from "../../api/pending/reports.ts";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { KIND_NAME, periodText } from "./kinds.ts";
import type { Notice } from "./ScheduleCard.tsx";
import { reportPath } from "./paths.ts";
import { reportListsKey } from "./queries.ts";
import { StatusBadge } from "./StatusBadge.tsx";
import { useDownload } from "./useDownload.ts";

/**
 * The generated reports as a table. Under 760 px each row becomes a card and each cell names its
 * column (`data-label`), so the page never scrolls sideways (design 「每頁都畫的狀態」).
 */
export function GeneratedReports({
  reports,
  canEdit,
  tenantId,
  onNotice,
}: {
  reports: readonly ReportSummary[];
  canEdit: boolean;
  tenantId: string;
  onNotice(notice: Notice): void;
}) {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = useReportApi();
  const queryClient = useQueryClient();
  const download = useDownload();
  const year = new Date().getFullYear();

  const retry = useMutation({
    mutationFn: (report: ReportSummary) => api.retryReport(report.id),
    onSuccess: async (report) => {
      await queryClient.invalidateQueries({ queryKey: reportListsKey(tenantId) });
      onNotice({
        kind: "ok",
        text: t("reports.message.retrying", {
          name: t(KIND_NAME[report.kind]),
          period: periodText(report.period_start, report.period_end, year),
        }),
      });
    },
    onError: (error) => onNotice({ kind: "error", text: errorMessage(error) }),
  });

  const save = (report: ReportSummary, format: DownloadFormat) =>
    download.mutate(
      { report, format },
      {
        onError: (error) =>
          onNotice({ kind: "error", text: t("reports.message.download_failed", { reason: errorMessage(error) }) }),
      },
    );

  return (
    <div className="rounded-lg border bg-card text-card-foreground">
      <table className="w-full text-sm max-md:block">
        <caption className="sr-only">{t("reports.generated.caption", { count: reports.length })}</caption>
        <thead className="bg-muted/60 text-left text-muted-foreground max-md:sr-only">
          <tr>
            <th scope="col" className="px-4 py-2.5 font-medium">
              {t("reports.generated.column.report")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("reports.generated.column.period")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("reports.generated.column.generated")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("reports.generated.column.status")}
            </th>
            <th scope="col" className="px-4 py-2.5">
              <span className="sr-only">{t("reports.generated.column.actions")}</span>
            </th>
          </tr>
        </thead>
        <tbody className="max-md:block">
          {reports.map((report) => {
            const name = t(KIND_NAME[report.kind]);
            const label = `${name} ${periodText(report.period_start, report.period_end, year)}`;
            const downloading = (format: DownloadFormat) =>
              download.isPending && download.variables?.report.id === report.id && download.variables.format === format;
            return (
              <tr key={report.id} className="border-t max-md:block max-md:space-y-1 max-md:px-4 max-md:py-3">
                <th scope="row" className="px-4 py-3 text-left font-normal max-md:block max-md:p-0">
                  {report.status === "ready" ? (
                    <Link
                      to={reportPath(report.id)}
                      className={cn("rounded-sm font-medium underline-offset-4 hover:underline", focusRing)}
                    >
                      {name}
                    </Link>
                  ) : (
                    <span className="font-medium">{name}</span>
                  )}
                </th>
                <td
                  data-label={t("reports.generated.column.period")}
                  className="px-3 py-3 whitespace-nowrap tabular-nums max-md:flex max-md:justify-between max-md:p-0 max-md:before:text-muted-foreground max-md:before:content-[attr(data-label)]"
                >
                  {periodText(report.period_start, report.period_end, year)}
                </td>
                <td
                  data-label={t("reports.generated.column.generated")}
                  className="px-3 py-3 whitespace-nowrap max-md:flex max-md:justify-between max-md:p-0 max-md:before:text-muted-foreground max-md:before:content-[attr(data-label)]"
                >
                  {report.generated_at ? (
                    <time dateTime={report.generated_at}>{formatDateTime(report.generated_at)}</time>
                  ) : (
                    t("reports.generated.just_started")
                  )}
                </td>
                <td
                  data-label={t("reports.generated.column.status")}
                  className="px-3 py-3 max-md:flex max-md:items-center max-md:justify-between max-md:p-0 max-md:before:text-muted-foreground max-md:before:content-[attr(data-label)]"
                >
                  <span className="grid justify-items-start gap-1 max-md:justify-items-end">
                    <StatusBadge status={report.status} />
                    {report.status === "failed" ? (
                      <span className="text-xs text-muted-foreground">
                        {t(report.failure === "query_timeout" ? "reports.failure.query_timeout" : "reports.failure.internal")}
                      </span>
                    ) : null}
                  </span>
                </td>
                <td className="px-4 py-3 max-md:block max-md:p-0 max-md:pt-1">
                  <div className="flex flex-wrap items-center justify-end gap-1 max-md:justify-start">
                    {report.status === "ready"
                      ? (["pdf", "csv"] as const).map((format) => (
                          <Button
                            key={format}
                            variant="ghost"
                            size="sm"
                            disabled={downloading(format)}
                            aria-label={t(format === "pdf" ? "reports.download.pdf_named" : "reports.download.csv_named", {
                              name: label,
                            })}
                            onClick={() => save(report, format)}
                          >
                            {downloading(format) ? <Spinner /> : <DownloadIcon aria-hidden="true" />}
                            {t(format === "pdf" ? "reports.download.pdf" : "reports.download.csv")}
                          </Button>
                        ))
                      : null}
                    {report.status === "failed" && canEdit ? (
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={retry.isPending && retry.variables?.id === report.id}
                        aria-label={t("reports.retry_named", { name: label })}
                        onClick={() => retry.mutate(report)}
                      >
                        <RotateCcwIcon aria-hidden="true" />
                        {t("reports.retry")}
                      </Button>
                    ) : null}
                  </div>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
