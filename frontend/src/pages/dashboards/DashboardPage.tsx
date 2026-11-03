import { useQuery } from "@tanstack/react-query";
import {
  ArchiveIcon,
  ArchiveRestoreIcon,
  CircleCheckIcon,
  EyeIcon,
  HistoryIcon,
  LayoutDashboardIcon,
  RefreshCwIcon,
  XIcon,
} from "lucide-react";
import { useId, useMemo, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import type { DashboardView, ShownItem } from "../../api/dashboards.ts";
import { ApiError, classifyError } from "../../api/errors.ts";
import { DASHBOARDS_PATH, dashboardPath } from "../../app/paths.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { DashboardComponent } from "../../dashboard/DashboardComponent.tsx";
import type { DashboardData } from "../../dashboard/data.ts";
import { dashboardFormat } from "../../dashboard/format.ts";
import { isPlatformComponent } from "../../dashboard/spec.ts";
import { componentDatasets } from "../../dashboard/uses.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { useSession } from "../../session/SessionProvider.tsx";
import { StatusPage } from "../StatusPage.tsx";
import { useSetArchived } from "./archive.ts";
import { CustomPageSlot } from "./CustomPageSlot.tsx";
import { creatorLabel } from "./creator.ts";
import { dataTimeRange, latestQueriedAt } from "./dataFacts.ts";
import { PageHeader } from "./PageHeader.tsx";
import { dashboardVersionKey } from "./queries.ts";
import { RestoreAction } from "./RestoreAction.tsx";
import { Tag } from "./Tag.tsx";
import { useDatasetRuns } from "./useDatasetRuns.ts";
import { VersionsPanel } from "./VersionsPanel.tsx";

/** The dashboard IDs the API hands out: lowercase hyphenated UUIDs (`DashboardId` in api/openapi.json). */
const DASHBOARD_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** `?version=<n>`: a version number, or the latest when it is absent or not one. */
function parseVersion(raw: string | null): number | undefined {
  if (raw === null || !/^[1-9][0-9]{0,8}$/.test(raw)) return undefined;
  return Number(raw);
}

/**
 * One dashboard (`/dashboards/:dashboardId`, checklist J4): the latest version, or the one
 * `?version=` names, drawn item by item in the order of its component list; every dataset of the
 * version runs as the viewer each time the page shows it, and "Refresh" runs them again. Its
 * versions, restoring an older one and archiving are here too, shown only to who may use them
 * (`permissions`). The layout follows docs/design/chat-dashboards/dashboard.html and
 * dashboard-phone.html; the version history, which the design does not draw, is in design.md.
 */
export function DashboardPage() {
  const { dashboardId = "" } = useParams();
  if (!DASHBOARD_ID.test(dashboardId)) {
    return <DashboardNotFound />;
  }
  // Another dashboard starts from a new page state: its own notices and open panels.
  return <Dashboard key={dashboardId.toLowerCase()} dashboardId={dashboardId.toLowerCase()} />;
}

/** What the last write said, until the next one or until it is dismissed. */
type Notice =
  | { kind: "restored"; from: number; version: number }
  | { kind: "archived" | "unarchived"; title: string }
  | { kind: "error"; error: unknown };

function Dashboard({ dashboardId }: { dashboardId: string }) {
  const { t, errorMessage } = useI18n();
  const api = useDashboardApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const [params] = useSearchParams();
  const requested = parseVersion(params.get("version"));
  const [versionsOpen, setVersionsOpen] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const versionsId = useId();

  const view = useQuery({
    queryKey: dashboardVersionKey(tenant, dashboardId, requested ?? "latest"),
    queryFn: ({ signal }) => api.getDashboard(dashboardId, requested, signal),
  });

  if (view.isPending) {
    return <DashboardSkeleton label={t("dashboard_page.loading")} />;
  }
  if (view.isError) {
    const error = view.error;
    if (error instanceof ApiError && error.body?.message_key === "dashboard.version_not_found") {
      return <VersionNotFound dashboardId={dashboardId} />;
    }
    if (classifyError(error) === "not_found") {
      return <DashboardNotFound />;
    }
    return (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("dashboard_page.load_failed", { reason: errorMessage(error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void view.refetch()} disabled={view.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }

  return (
    <VersionView
      // A version's results are its own: another version starts with no results.
      key={view.data.version.version}
      view={view.data}
      versionsOpen={versionsOpen}
      onToggleVersions={() => setVersionsOpen((open) => !open)}
      versionsId={versionsId}
      notice={notice}
      onNotice={setNotice}
    />
  );
}

/** A version's component list; a version written before 2026-11-01 has only its page (`Spec::layout`). */
function layoutOf(version: DashboardView["version"]): ShownItem[] {
  const components = version.spec.components ?? [];
  if (components.length > 0) return components;
  return version.page != null ? [{ kind: "custom_page" }] : [];
}

/** A KPI and a status sit side by side; charts, tables and pages take the whole width. */
function itemClass(item: ShownItem): string {
  return item.kind === "kpi" || item.kind === "status" ? "min-w-0" : "min-w-0 sm:col-span-2 xl:col-span-4";
}

function VersionView(props: {
  view: DashboardView;
  versionsOpen: boolean;
  onToggleVersions(): void;
  versionsId: string;
  notice: Notice | null;
  onNotice(notice: Notice | null): void;
}) {
  const { view, notice, onNotice } = props;
  const { t, errorMessage, formatDateTime } = useI18n();
  const { locale, timeZone } = usePreferences();
  const { session } = useSession();
  const { dashboard, version, permissions } = view;
  const latest = dashboard.latest_version;
  const isLatest = version.version === latest;
  const archived = dashboard.archived_at != null;
  const viewer = !permissions.can_publish && !permissions.can_restore && !permissions.can_archive;
  const archive = useSetArchived();

  const items = useMemo(() => layoutOf(version), [version]);
  // The components' datasets first, in the order they are shown; then the rest (a custom page's).
  const datasets = useMemo(() => {
    const computations = version.spec.computations ?? [];
    const shown = items.flatMap((item) =>
      item.kind !== "unsupported" && isPlatformComponent(item) ? componentDatasets(item, { computations }) : [],
    );
    const declared = version.spec.datasets.map((dataset) => dataset.name);
    return [...new Set([...shown.filter((name) => declared.includes(name)), ...declared])];
  }, [items, version]);
  const runs = useDatasetRuns(dashboard.dashboard_id, version.version, datasets);
  const data = useMemo<DashboardData>(
    () => ({
      datasets: version.spec.datasets,
      computations: version.spec.computations ?? [],
      constants: version.spec.constants ?? [],
      results: runs.results,
    }),
    [version, runs.results],
  );

  const queriedAt = latestQueriedAt(data);
  const range = dataTimeRange(data);
  const format = useMemo(() => dashboardFormat(locale, timeZone), [locale, timeZone]);

  const setArchived = (next: boolean) => {
    onNotice(null);
    archive.mutate(
      { dashboardId: dashboard.dashboard_id, archived: next },
      {
        onSuccess: () => onNotice({ kind: next ? "archived" : "unarchived", title: version.title }),
        onError: (error) => onNotice({ kind: "error", error }),
      },
    );
  };

  const who = creatorLabel(version.created_by, session?.user.id, t);
  const written = formatDateTime(version.created_at);
  const tags = (
    <>
      {dashboard.kind === "draft" ? (
        <Tag>{t("dashboard_page.status_draft", { version: version.version })}</Tag>
      ) : isLatest ? (
        <Tag icon={<CircleCheckIcon aria-hidden="true" className="text-success" />}>
          {t("dashboard_page.status_published", { version: version.version })}
        </Tag>
      ) : (
        <Tag tone="warning" icon={<HistoryIcon aria-hidden="true" />}>
          {t("dashboard_page.status_old", { version: version.version, latest })}
        </Tag>
      )}
      {archived ? <Tag icon={<ArchiveIcon aria-hidden="true" />}>{t("dashboards.archived")}</Tag> : null}
    </>
  );
  const meta = (
    <div className="grid gap-1">
      {version.description ? <p className="text-pretty">{version.description}</p> : null}
      <p className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
        <span className="tabular-nums">
          {queriedAt !== null ? t("dashboard_page.data_time", { time: formatDateTime(queriedAt) }) : null}
        </span>
        {/* Always in the page, so the screen reader announces the text when it appears. */}
        <span role="status" className="inline-flex items-center gap-1.5 text-foreground">
          {runs.refreshing ? (
            <>
              <Spinner />
              {t("dashboard_page.refreshing")}
            </>
          ) : null}
        </span>
        <span aria-hidden="true">·</span>
        <span>{t("dashboard_page.requeried")}</span>
        <span aria-hidden="true">·</span>
        <span className="tabular-nums">{t(`dashboard_page.written.${version.origin}`, { who, time: written })}</span>
      </p>
    </div>
  );

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={version.title}
        crumbs={[{ label: t("dashboards.title"), to: DASHBOARDS_PATH }]}
        tags={tags}
        meta={meta}
        actions={
          <>
            <Button variant="outline" onClick={runs.refresh} disabled={runs.running}>
              <RefreshCwIcon className={runs.running ? "animate-spin" : undefined} />
              {t("dashboard_page.refresh")}
            </Button>
            <Button
              variant="outline"
              aria-expanded={props.versionsOpen}
              aria-controls={props.versionsId}
              onClick={props.onToggleVersions}
            >
              <HistoryIcon />
              {t("dashboard_page.versions")}
            </Button>
            {permissions.can_archive ? (
              <Button variant="ghost" disabled={archive.isPending} onClick={() => setArchived(!archived)}>
                {archived ? <ArchiveRestoreIcon /> : <ArchiveIcon />}
                {t(archived ? "dashboards.unarchive" : "dashboards.archive")}
              </Button>
            ) : null}
          </>
        }
      />

      {viewer ? (
        <Alert role="status" className="grid-cols-[1rem_1fr]">
          <EyeIcon aria-hidden="true" />
          <AlertTitle>{t("dashboard_page.viewer.title")}</AlertTitle>
          <AlertDescription>
            <p>{t("dashboard_page.viewer.body")}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      {notice !== null ? (
        <NoticeBanner
          notice={notice}
          onDismiss={() => onNotice(null)}
          message={noticeMessage(notice, t, errorMessage)}
        />
      ) : null}

      {!isLatest ? (
        <Alert role="status" className="grid-cols-[1fr_auto] items-center gap-x-3 gap-y-2 max-sm:grid-cols-1">
          <AlertDescription className="col-start-1 text-foreground">
            <p>{t("dashboard_page.old_version", { version: version.version, latest })}</p>
          </AlertDescription>
          <div className="flex flex-wrap gap-2 sm:col-start-2 sm:row-start-1">
            <Button asChild variant="outline" size="sm">
              <Link to={dashboardPath(dashboard.dashboard_id)}>{t("dashboard_page.show_latest")}</Link>
            </Button>
            {permissions.can_restore && !archived ? (
              <RestoreAction
                dashboardId={dashboard.dashboard_id}
                version={version.version}
                latest={latest}
                onRestored={(created) => onNotice({ kind: "restored", ...created })}
              />
            ) : null}
          </div>
        </Alert>
      ) : null}

      {archived ? (
        <Alert role="status" className="grid-cols-[1rem_1fr] items-start">
          <ArchiveIcon />
          <AlertDescription className="text-foreground">
            <p>{t("dashboard_page.archived_notice")}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      {props.versionsOpen ? (
        <VersionsPanel
          id={props.versionsId}
          dashboardId={dashboard.dashboard_id}
          shown={version.version}
          latest={latest}
          canRestore={permissions.can_restore && !archived}
          onRestored={(created) => onNotice({ kind: "restored", ...created })}
          onClose={props.onToggleVersions}
        />
      ) : null}

      {range !== null ? (
        <p className="text-sm text-muted-foreground tabular-nums">
          {t("dashboard_page.data_range", {
            from: format.wallTime(range.from, range.dateOnly),
            to: format.wallTime(range.to, range.dateOnly),
          })}
        </p>
      ) : null}

      <section
        aria-label={t("dashboard_page.items_label")}
        aria-busy={runs.refreshing}
        // The earlier content stays while the datasets run again, dimmed (opacity-refetch in the
        // design system); the page never goes back to skeletons.
        className={cn(
          "grid gap-4 transition-opacity duration-150 sm:grid-cols-2 xl:grid-cols-4",
          runs.refreshing && "opacity-60",
        )}
      >
        {items.length === 0 ? (
          <p className="text-sm text-muted-foreground sm:col-span-2 xl:col-span-4">{t("dashboard_page.no_items")}</p>
        ) : null}
        {items.map((item, index) => (
          // The list is a version's, fixed for the life of this view: its index is its identity.
          // biome-ignore lint/suspicious/noArrayIndexKey: the items of a version never move
          <div key={index} className={itemClass(item)}>
            {item.kind === "unsupported" ? (
              <UnsupportedItem />
            ) : isPlatformComponent(item) ? (
              <DashboardComponent spec={item} data={data} onRetry={runs.retry} />
            ) : (
              <CustomPageSlot dashboardId={dashboard.dashboard_id} version={version} data={data} />
            )}
          </div>
        ))}
      </section>
    </div>
  );
}

function noticeMessage(
  notice: Notice,
  t: ReturnType<typeof useI18n>["t"],
  errorMessage: ReturnType<typeof useI18n>["errorMessage"],
): string {
  switch (notice.kind) {
    case "restored":
      return t("dashboard_page.restored", { from: notice.from, version: notice.version });
    case "archived":
      return t("dashboards.message.archived", { title: notice.title });
    case "unarchived":
      return t("dashboards.message.unarchived", { title: notice.title });
    case "error":
      return errorMessage(notice.error);
  }
}

function NoticeBanner({ notice, message, onDismiss }: { notice: Notice; message: string; onDismiss(): void }) {
  const { t } = useI18n();
  const error = notice.kind === "error";
  return (
    <Alert
      variant={error ? "destructive" : "default"}
      role={error ? "alert" : "status"}
      className="grid-cols-[1fr_auto] items-center"
    >
      <AlertDescription className={cn("col-start-1", error ? undefined : "text-foreground")}>
        <p>{message}</p>
      </AlertDescription>
      <Button variant="ghost" size="icon" aria-label={t("dashboard_page.dismiss")} onClick={onDismiss}>
        <XIcon />
      </Button>
    </Alert>
  );
}

/**
 * The first load: the shape of the page as skeletons (docs/design/chat-dashboards/states.html,
 * state 5), announced once. Running the datasets again later never comes back to this.
 */
function DashboardSkeleton({ label }: { label: string }) {
  const bar = "animate-pulse rounded bg-muted";
  return (
    <div aria-busy="true" className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <p role="status" className="sr-only">
        {label}
      </p>
      <div aria-hidden="true" className="grid gap-3">
        <span className={cn(bar, "h-4 w-48")} />
        <span className={cn(bar, "h-8 w-2/3 max-w-md")} />
        <span className={cn(bar, "h-4 w-1/2 max-w-sm")} />
      </div>
      <div aria-hidden="true" className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
        {[0, 1, 2, 3].map((card) => (
          <div key={card} className="grid gap-3 rounded-xl border bg-card p-4">
            <span className={cn(bar, "h-3 w-1/2")} />
            <span className={cn(bar, "h-8 w-1/3")} />
            <span className={cn(bar, "h-3 w-2/3")} />
          </div>
        ))}
        <div className="grid gap-3 rounded-xl border bg-card p-4 sm:col-span-2 xl:col-span-4">
          <span className={cn(bar, "h-4 w-1/4")} />
          <span className={cn(bar, "h-48 w-full")} />
        </div>
      </div>
    </div>
  );
}

/** An item of a newer kind, or with a setting, this version of the app does not know. */
function UnsupportedItem() {
  const { t } = useI18n();
  const titleId = useId();
  return (
    <section
      aria-labelledby={titleId}
      data-component="unsupported"
      className="grid justify-items-start gap-2 rounded-xl border border-dashed bg-card px-4 py-5 text-card-foreground sm:px-5"
    >
      <h3 id={titleId} className="text-base font-semibold">
        {t("dashboard_page.unsupported.title")}
      </h3>
      <p className="text-sm text-muted-foreground text-pretty">{t("dashboard_page.unsupported.body")}</p>
    </section>
  );
}

/** No such dashboard for this user in the active tenant: the next step is the list. */
function DashboardNotFound() {
  const { t } = useI18n();
  return (
    <StatusPage
      title={t("dashboard_page.not_found.title")}
      icon={<LayoutDashboardIcon />}
      actions={
        <Button asChild variant="outline">
          <Link to={DASHBOARDS_PATH}>{t("dashboard_page.back_to_list")}</Link>
        </Button>
      }
    >
      <p>{t("dashboard_page.not_found.body")}</p>
    </StatusPage>
  );
}

/** The dashboard exists but `?version=` names none of its versions. */
function VersionNotFound({ dashboardId }: { dashboardId: string }) {
  const { t } = useI18n();
  return (
    <StatusPage
      title={t("dashboard_page.version_not_found.title")}
      icon={<HistoryIcon />}
      actions={
        <Button asChild variant="outline">
          <Link to={dashboardPath(dashboardId)}>{t("dashboard_page.show_latest")}</Link>
        </Button>
      }
    >
      <p>{t("dashboard.version_not_found")}</p>
    </StatusPage>
  );
}
