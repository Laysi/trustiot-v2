import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { EyeIcon, FileTextIcon, PlusIcon } from "lucide-react";
import { type ReactNode, useId, useState } from "react";
import { Link, useSearchParams } from "react-router";

import { useReportApi } from "../../api/pending/ReportApiProvider.tsx";
import { FlashNotice } from "../../components/FlashNotice.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { isNotInPlan, useReportRole } from "./access.ts";
import { GeneratedReports } from "./GeneratedReports.tsx";
import { isReportKind, KIND_NAME } from "./kinds.ts";
import { PlanWall } from "./PlanWall.tsx";
import { NEW_SCHEDULE_PATH, newScheduleFor } from "./paths.ts";
import { reportListKey, schedulesKey } from "./queries.ts";
import { type Notice, ScheduleCard } from "./ScheduleCard.tsx";

/** `?kind=` narrows the generated reports to one type, so a link or a reload keeps the choice. */
const KIND_PARAM = "kind";
const PAGE_SIZE = 20;
/** How often the list is read again while a report is being made. */
const GENERATING_POLL_MS = 5000;

/**
 * The reports page (`/reports`, Laysi/trustiot-v2#174): the schedules, and the reports they made,
 * with their downloads. The layout follows docs/design/ms4-ms5/reports.html: loading, empty,
 * failed, the Starter upgrade wall and the viewer's read-only version are all drawn there.
 */
export function ReportsPage() {
  const { t, errorMessage } = useI18n();
  const api = useReportApi();
  const { canEdit, tenantId } = useReportRole();
  const [params, setParams] = useSearchParams();
  const kindParam = params.get(KIND_PARAM);
  const kind = isReportKind(kindParam) ? kindParam : undefined;
  const [notice, setNotice] = useState<Notice | null>(null);
  const filterId = useId();

  const schedules = useQuery({
    queryKey: schedulesKey(tenantId),
    queryFn: ({ signal }) => api.listSchedules(signal),
  });
  const reports = useInfiniteQuery({
    queryKey: reportListKey(tenantId, kind ?? "all"),
    queryFn: ({ pageParam, signal }) =>
      api.listReports(
        { limit: PAGE_SIZE, ...(kind ? { kind } : {}), ...(pageParam !== undefined ? { cursor: pageParam } : {}) },
        signal,
      ),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    // A report that is being made turns ready or failed on the server: the list follows it.
    refetchInterval: (query) =>
      query.state.data?.pages.some((page) => page.reports.some((report) => report.status === "generating"))
        ? GENERATING_POLL_MS
        : false,
  });

  const setKind = (value: string) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (isReportKind(value)) next.set(KIND_PARAM, value);
        else next.delete(KIND_PARAM);
        return next;
      },
      { replace: true },
    );

  const error = schedules.error ?? reports.error;
  const loadedReports = reports.data?.pages.flatMap((page) => page.reports) ?? [];
  const notInPlan = isNotInPlan(schedules.error) || isNotInPlan(reports.error);

  let body: ReactNode;
  if (notInPlan) {
    body = <PlanWall />;
  } else if (schedules.isPending || reports.isPending) {
    body = <LoadingSkeleton label={t("reports.loading")} />;
  } else if (schedules.data === undefined || reports.data === undefined) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1rem_1fr_auto] items-center gap-x-3">
        <FileTextIcon aria-hidden="true" />
        <AlertDescription className="col-start-2">
          <p>{t("reports.load_failed", { reason: errorMessage(error) })}</p>
          <p>{t("reports.load_failed_note")}</p>
        </AlertDescription>
        <Button
          variant="outline"
          onClick={() => {
            void schedules.refetch();
            void reports.refetch();
          }}
        >
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (schedules.data.length === 0 && loadedReports.length === 0 && kind === undefined) {
    body = <EmptyState canEdit={canEdit} />;
  } else {
    body = (
      <div className="grid gap-7">
        <section aria-labelledby="reports-schedules" className="grid gap-3">
          <h2 id="reports-schedules" className="text-lg font-semibold">
            {t("reports.schedules.heading")}
          </h2>
          {schedules.data.length === 0 ? (
            <p className="rounded-lg border px-4 py-6 text-sm text-muted-foreground">
              {t(canEdit ? "reports.schedules.none" : "reports.empty.viewer")}
            </p>
          ) : (
            <div className="grid gap-4 md:grid-cols-2">
              {schedules.data.map((schedule) => (
                <ScheduleCard
                  key={schedule.id}
                  schedule={schedule}
                  canEdit={canEdit}
                  tenantId={tenantId}
                  onNotice={setNotice}
                />
              ))}
            </div>
          )}
        </section>

        <section aria-labelledby="reports-generated" className="grid gap-3">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h2 id="reports-generated" className="text-lg font-semibold">
              {t("reports.generated.heading")}
            </h2>
            <div className="w-full sm:w-52">
              <label htmlFor={filterId} className="sr-only">
                {t("reports.generated.filter_label")}
              </label>
              <NativeSelect id={filterId} value={kind ?? ""} onChange={(event) => setKind(event.target.value)}>
                <option value="">{t("reports.generated.filter_all")}</option>
                <option value="weekly_energy">{t(KIND_NAME.weekly_energy)}</option>
                <option value="monthly_carbon">{t(KIND_NAME.monthly_carbon)}</option>
              </NativeSelect>
            </div>
          </div>
          {loadedReports.length === 0 ? (
            <p className="rounded-lg border px-4 py-6 text-sm text-muted-foreground">
              {t(kind ? "reports.generated.empty_filtered" : "reports.generated.empty")}
            </p>
          ) : (
            <GeneratedReports reports={loadedReports} canEdit={canEdit} tenantId={tenantId} onNotice={setNotice} />
          )}
          {reports.hasNextPage ? (
            <div>
              <Button
                variant="outline"
                onClick={() => void reports.fetchNextPage()}
                disabled={reports.isFetchingNextPage}
              >
                {t(reports.isFetchingNextPage ? "reports.loading_more" : "reports.load_more")}
              </Button>
            </div>
          ) : null}
          {reports.isFetchNextPageError ? (
            <Alert variant="destructive">
              <AlertDescription>
                <p>{t("reports.load_failed", { reason: errorMessage(reports.error) })}</p>
              </AlertDescription>
            </Alert>
          ) : null}
        </section>
      </div>
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={t("reports.title")}
        meta={t("reports.description")}
        actions={
          canEdit && !notInPlan ? (
            <Button asChild>
              <Link to={NEW_SCHEDULE_PATH}>
                <PlusIcon aria-hidden="true" />
                {t("reports.new_schedule")}
              </Link>
            </Button>
          ) : null
        }
      />
      <FlashNotice />
      {!canEdit && !notInPlan ? (
        <Alert role="status" className="grid-cols-[1rem_1fr]">
          <EyeIcon aria-hidden="true" />
          <AlertTitle>{t("reports.viewer.title")}</AlertTitle>
          <AlertDescription>
            <p>{t("reports.viewer.body")}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {body}
      {/* What a write did, read out beside the list. */}
      <p
        role="status"
        className={cn("min-h-5 text-sm", notice?.kind === "error" ? "text-destructive" : "text-foreground")}
      >
        {notice?.text}
      </p>
    </div>
  );
}

function EmptyState({ canEdit }: { canEdit: boolean }) {
  const { t } = useI18n();
  return (
    <div role="status" className="grid justify-items-center gap-3 rounded-lg border px-4 py-12 text-center">
      <FileTextIcon aria-hidden="true" className="size-6 text-muted-foreground" />
      <h2 className="text-lg font-semibold">{t("reports.empty.title")}</h2>
      <p className="max-w-xl text-sm text-muted-foreground">
        {t(canEdit ? "reports.empty.body" : "reports.empty.viewer")}
      </p>
      {canEdit ? (
        <div className="flex flex-wrap justify-center gap-2">
          <Button asChild>
            <Link to={newScheduleFor("weekly_energy")}>{t("reports.empty.weekly")}</Link>
          </Button>
          <Button asChild variant="outline">
            <Link to={newScheduleFor("monthly_carbon")}>{t("reports.empty.monthly")}</Link>
          </Button>
        </div>
      ) : null}
    </div>
  );
}

/** Cards that stand in for the schedules while they load; announced once. */
function LoadingSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 md:grid-cols-2">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1].map((card) => (
        <div key={card} aria-hidden="true" className="grid gap-3 rounded-lg border bg-card p-4">
          <span className="h-4 w-1/2 animate-pulse rounded bg-muted" />
          <span className="h-3 w-4/5 animate-pulse rounded bg-muted" />
          <span className="h-3 w-2/5 animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}
