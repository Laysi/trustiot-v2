import { useQuery } from "@tanstack/react-query";
import { CompassIcon, DownloadIcon } from "lucide-react";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { classifyError } from "../../api/errors.ts";
import { useReportApi } from "../../api/pending/ReportApiProvider.tsx";
import type { DownloadFormat, ReportDetail } from "../../api/pending/reports.ts";
import { devicePath } from "../../app/paths.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { StatusPage } from "../StatusPage.tsx";
import { AskAiButton } from "./AskAiButton.tsx";
import { isNotInPlan, useReportRole } from "./access.ts";
import { DailyChart, DailyTable } from "./DailyChart.tsx";
import { formatAmount, formatChange, formatMonth } from "./format.ts";
import { CARBON_FACTOR, KIND_NAME, periodText } from "./kinds.ts";
import { PlanWall } from "./PlanWall.tsx";
import { REPORTS_PATH } from "./paths.ts";
import { reportKey } from "./queries.ts";
import { useDownload } from "./useDownload.ts";

/**
 * One report (`/reports/:reportId`): the key figures, the daily chart, each device and the
 * footnote on where the numbers come from (docs/design/ms4-ms5/design.md 「檢視報表」). A report is
 * a stored dataset (decision Q6), so the page draws what was saved and never queries the data again.
 */
export function ReportPage() {
  const { reportId = "" } = useParams();
  const { t, errorMessage } = useI18n();
  const api = useReportApi();
  const { tenantId } = useReportRole();
  const report = useQuery({
    queryKey: reportKey(tenantId, reportId),
    queryFn: ({ signal }) => api.getReport(reportId, signal),
  });
  const crumbs = [{ label: t("reports.title"), to: REPORTS_PATH }];

  if (report.isPending) {
    return (
      <div className="grid gap-4" aria-busy="true">
        <p role="status" className="sr-only">
          {t("reports.view.loading")}
        </p>
        <span aria-hidden="true" className="h-8 w-1/3 animate-pulse rounded bg-muted" />
        <span aria-hidden="true" className="h-24 animate-pulse rounded bg-muted" />
        <span aria-hidden="true" className="h-56 animate-pulse rounded bg-muted" />
      </div>
    );
  }
  if (report.data === undefined) {
    if (isNotInPlan(report.error)) {
      return (
        <div className="grid gap-4">
          <PageHeader title={t("reports.title")} />
          <PlanWall />
        </div>
      );
    }
    if (classifyError(report.error) === "not_found") {
      return (
        <StatusPage
          title={t("reports.not_found.title")}
          icon={<CompassIcon />}
          actions={
            <Button asChild variant="outline">
              <Link to={REPORTS_PATH}>{t("reports.not_found.back")}</Link>
            </Button>
          }
        >
          <p>{t("reports.not_found.body")}</p>
        </StatusPage>
      );
    }
    return (
      <div className="grid gap-4">
        <PageHeader title={t("reports.title")} crumbs={crumbs} />
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{t("reports.view.load_failed", { reason: errorMessage(report.error) })}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void report.refetch()} disabled={report.isFetching}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      </div>
    );
  }
  return <ReportView report={report.data} />;
}

function titleOf(
  report: ReportDetail,
  t: ReturnType<typeof useI18n>["t"],
  locale: ReturnType<typeof useI18n>["locale"],
) {
  const name = t(KIND_NAME[report.kind]);
  const period =
    report.kind === "monthly_carbon"
      ? formatMonth(report.period_start, locale)
      : periodText(report.period_start, report.period_end, new Date().getFullYear());
  return `${name} ${period}`;
}

function ReportView({ report }: { report: ReportDetail }) {
  const { t, locale, errorMessage, formatDateTime } = useI18n();
  const download = useDownload();
  const [showTable, setShowTable] = useState(false);
  const crumbs = [{ label: t("reports.title"), to: REPORTS_PATH }];
  const title = titleOf(report, t, locale);

  if (report.content === null) {
    return (
      <div className="grid gap-4">
        <PageHeader title={title} crumbs={crumbs} />
        <Alert role="status" variant={report.status === "failed" ? "destructive" : "default"}>
          <AlertDescription>
            <p>
              {report.status === "failed"
                ? t("reports.view.failed", {
                    reason: t(
                      report.failure === "query_timeout" ? "reports.failure.query_timeout" : "reports.failure.internal",
                    ),
                  })
                : t("reports.view.generating")}
            </p>
          </AlertDescription>
        </Alert>
      </div>
    );
  }

  const { content } = report;
  const { kpis } = content;
  const year = new Date().getFullYear();
  const scopeText =
    content.scope.kind === "all"
      ? t("reports.schedules.scope_all")
      : t("reports.schedules.scope_devices", { count: content.scope.device_ids.length });
  const monthly = report.kind === "monthly_carbon";
  const factor = String(kpis.carbon_factor || CARBON_FACTOR);
  const saving = (format: DownloadFormat) => download.isPending && download.variables?.format === format;
  const about = `${title}, ${periodText(report.period_start, report.period_end, year)}`;

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-5">
      <PageHeader
        title={title}
        crumbs={crumbs}
        meta={t("reports.view.lede", {
          scope: scopeText,
          period: periodText(report.period_start, report.period_end, year),
          zone: content.time_zone,
          time: report.generated_at ? formatDateTime(report.generated_at) : "",
        })}
        actions={(["pdf", "csv"] as const).map((format) => (
          <Button
            key={format}
            variant="outline"
            disabled={saving(format)}
            aria-label={t(format === "pdf" ? "reports.download.pdf_named" : "reports.download.csv_named", {
              name: title,
            })}
            onClick={() => download.mutate({ report, format })}
          >
            {saving(format) ? <Spinner /> : <DownloadIcon aria-hidden="true" />}
            {t(format === "pdf" ? "reports.download.pdf" : "reports.download.csv")}
          </Button>
        ))}
      />
      {download.isError ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{t("reports.message.download_failed", { reason: errorMessage(download.error) })}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      <section
        aria-label={t("reports.view.kpis_label")}
        className={cn("grid gap-3 sm:grid-cols-2", monthly ? "lg:grid-cols-4" : "lg:grid-cols-3")}
      >
        <Kpi label={t("reports.kpi.total")} value={`${formatAmount(kpis.total_kwh, locale)} kWh`} />
        <Kpi
          label={t("reports.kpi.carbon")}
          value={`${formatAmount(kpis.carbon_kg, locale)} kg`}
          note={t("reports.kpi.carbon_note", { factor })}
        />
        <Kpi
          label={t(monthly ? "reports.kpi.mom" : "reports.kpi.wow")}
          value={formatChange(kpis.previous_period_change, locale)}
          note={
            kpis.previous_period_kwh === null
              ? t("reports.kpi.no_comparison")
              : t(monthly ? "reports.kpi.mom_note" : "reports.kpi.wow_note", {
                  value: formatAmount(kpis.previous_period_kwh, locale),
                })
          }
        />
        {monthly ? (
          <Kpi
            label={t("reports.kpi.yoy")}
            value={formatChange(kpis.previous_year_change, locale)}
            note={
              kpis.previous_year_kwh === null
                ? t("reports.kpi.no_comparison")
                : t("reports.kpi.yoy_note", { value: formatAmount(kpis.previous_year_kwh, locale) })
            }
          />
        ) : null}
      </section>

      <div className="flex flex-wrap gap-2">
        <AskAiButton question={t("reports.ask.explain")} about={about} />
        <AskAiButton question={t("reports.ask.why_changed")} about={about} />
        <AskAiButton question={t("reports.ask.save_energy")} about={about} />
      </div>

      <section
        aria-labelledby="report-daily"
        className="grid gap-3 rounded-xl border bg-card p-4 text-card-foreground shadow-sm"
      >
        <div className="flex flex-wrap items-start justify-between gap-2">
          <div>
            <h2 id="report-daily" className="font-semibold">
              {t("reports.daily.heading")}
            </h2>
            <p className="text-sm text-muted-foreground">{t("reports.daily.sub")}</p>
          </div>
          <Button
            variant="ghost"
            size="sm"
            aria-pressed={showTable}
            onClick={() => setShowTable((current) => !current)}
          >
            {t(showTable ? "reports.daily.show_chart" : "reports.daily.show_table")}
          </Button>
        </div>
        {showTable ? <DailyTable days={content.daily} /> : <DailyChart days={content.daily} />}
      </section>

      <section
        aria-labelledby="report-devices"
        className="grid gap-2 rounded-xl border bg-card p-4 text-card-foreground shadow-sm"
      >
        <h2 id="report-devices" className="font-semibold">
          {t("reports.devices.heading")}
        </h2>
        <table className="w-full text-sm">
          <caption className="sr-only">{t("reports.devices.caption")}</caption>
          <thead className="text-left text-muted-foreground">
            <tr>
              <th scope="col" className="py-1.5 font-medium">
                {t("reports.devices.column.device")}
              </th>
              <th scope="col" className="py-1.5 text-right font-medium">
                {t("reports.devices.column.kwh")}
              </th>
              <th scope="col" className="py-1.5 text-right font-medium">
                {t("reports.devices.column.carbon")}
              </th>
              <th scope="col" className="py-1.5 text-right font-medium max-sm:hidden">
                {t("reports.devices.column.change")}
              </th>
            </tr>
          </thead>
          <tbody>
            {content.devices.map((device) => (
              <tr key={device.device_id} className="border-t">
                <th scope="row" className="py-1.5 text-left font-normal">
                  <Link
                    to={devicePath(device.device_id)}
                    className={cn("rounded-sm underline-offset-4 hover:underline", focusRing)}
                  >
                    {device.name}
                  </Link>
                </th>
                <td className="py-1.5 text-right tabular-nums">{formatAmount(device.kwh, locale)}</td>
                <td className="py-1.5 text-right tabular-nums">{formatAmount(device.carbon_kg, locale)}</td>
                <td className="py-1.5 text-right tabular-nums max-sm:hidden">{formatChange(device.change, locale)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>

      <p className="text-xs text-muted-foreground">{t("reports.footnote", { factor })}</p>
    </div>
  );
}

function Kpi({ label, value, note }: { label: string; value: string; note?: string }) {
  return (
    <div className="grid content-start gap-0.5 rounded-xl border bg-card p-4 text-card-foreground shadow-sm">
      <span className="text-sm text-muted-foreground">{label}</span>
      <span className="text-2xl font-semibold tabular-nums">{value}</span>
      {note ? <span className="text-xs text-muted-foreground">{note}</span> : null}
    </div>
  );
}
