import { useInfiniteQuery } from "@tanstack/react-query";
import { ArchiveIcon, LayoutDashboardIcon } from "lucide-react";
import { type ReactNode, useId } from "react";
import { Link, useSearchParams } from "react-router";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import type { DashboardSummary } from "../../api/dashboards.ts";
import { dashboardPath } from "../../app/paths.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "./PageHeader.tsx";
import { dashboardListKey } from "./queries.ts";

/** `?archived=1` lists archived dashboards too, so a link or a reload keeps the choice. */
const ARCHIVED_PARAM = "archived";

/**
 * The tenant's saved dashboards (`/dashboards`, checklist J4), the most recently updated first:
 * each links to its own page. The list follows docs/design/tenant-admin/ (a table on wide
 * screens, cards on narrow ones; loading, empty and error states); dashboards are made and changed
 * in a conversation with the assistant, so the page has no "new" button.
 */
export function DashboardsPage() {
  const { t, errorMessage } = useI18n();
  const api = useDashboardApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const [params, setParams] = useSearchParams();
  const includeArchived = params.get(ARCHIVED_PARAM) === "1";
  const toggleId = useId();

  const list = useInfiniteQuery({
    queryKey: dashboardListKey(tenant, includeArchived),
    queryFn: ({ pageParam, signal }) =>
      api.listDashboards({ includeArchived, ...(pageParam !== undefined ? { cursor: pageParam } : {}) }, signal),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });

  const setIncludeArchived = (value: boolean) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value) next.set(ARCHIVED_PARAM, "1");
        else next.delete(ARCHIVED_PARAM);
        return next;
      },
      { replace: true },
    );

  const dashboards = list.data?.pages.flatMap((page) => page.dashboards) ?? [];

  let body: ReactNode;
  if (list.isPending) {
    body = <ListSkeleton label={t("dashboards.loading")} />;
  } else if (list.data === undefined) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("dashboards.load_failed", { reason: errorMessage(list.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (dashboards.length === 0) {
    body = (
      <EmptyState
        icon={<LayoutDashboardIcon />}
        title={t(includeArchived ? "dashboards.empty.title_all" : "dashboards.empty.title")}
        action={
          <Button asChild variant="outline">
            <Link to="/">{t("page.action.home")}</Link>
          </Button>
        }
      >
        <p>{t("dashboards.empty.body")}</p>
      </EmptyState>
    );
  } else {
    body = (
      <div className="grid gap-4">
        <DashboardTable dashboards={dashboards} />
        {list.hasNextPage ? (
          <div>
            <Button variant="outline" onClick={() => void list.fetchNextPage()} disabled={list.isFetchingNextPage}>
              {list.isFetchingNextPage ? t("dashboards.loading_more") : t("dashboards.load_more")}
            </Button>
          </div>
        ) : null}
        {list.isFetchNextPageError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{t("dashboards.load_failed", { reason: errorMessage(list.error) })}</p>
            </AlertDescription>
          </Alert>
        ) : null}
      </div>
    );
  }

  return (
    <div className="grid gap-6">
      <PageHeader title={t("dashboards.title")} description={t("dashboards.description")} />
      <div className="flex flex-wrap items-center gap-2">
        <input
          id={toggleId}
          type="checkbox"
          className={cn("size-4 accent-primary", focusRing)}
          checked={includeArchived}
          onChange={(event) => setIncludeArchived(event.target.checked)}
        />
        <label htmlFor={toggleId} className="text-sm">
          {t("dashboards.show_archived")}
        </label>
      </div>
      {body}
    </div>
  );
}

/** Rows that stand in for the list while it loads; announced once, not row by row. */
function ListSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 rounded-xl border bg-card px-4 py-5 shadow-sm sm:px-6">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1, 2].map((row) => (
        <div key={row} aria-hidden="true" className="flex items-center justify-between gap-4">
          <span className="h-3 w-2/5 animate-pulse rounded bg-muted" />
          <span className="h-3 w-1/6 animate-pulse rounded bg-muted" />
          <span className="h-3 w-1/5 animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}

function ArchivedBadge() {
  const { t } = useI18n();
  return (
    <span className="inline-flex h-5.5 items-center gap-1.5 rounded-md bg-secondary px-2 text-xs font-medium whitespace-nowrap text-secondary-foreground">
      <ArchiveIcon aria-hidden="true" className="size-3.5" />
      {t("dashboards.archived")}
    </span>
  );
}

function DashboardLink({ dashboard }: { dashboard: DashboardSummary }) {
  return (
    <Link
      to={dashboardPath(dashboard.dashboard_id)}
      className={cn(
        "rounded-sm font-medium wrap-anywhere underline decoration-transparent underline-offset-3 hover:decoration-current",
        focusRing,
      )}
    >
      {dashboard.title}
    </Link>
  );
}

/**
 * The dashboards as a table on wide screens and as a list of cards on narrow ones, from the same
 * rows; CSS hides one of the two, so only the shown one is in the accessibility tree.
 */
function DashboardTable({ dashboards }: { dashboards: readonly DashboardSummary[] }) {
  const { t, formatDateTime } = useI18n();
  return (
    <section
      aria-label={t("dashboards.list_label")}
      className="rounded-xl border bg-card text-card-foreground shadow-sm"
    >
      <table className="hidden w-full text-sm md:table">
        <caption className="sr-only">{t("dashboards.caption", { count: dashboards.length })}</caption>
        <thead>
          <tr className="border-b text-left text-muted-foreground">
            <th scope="col" className="px-6 py-2.5 font-medium">
              {t("dashboards.column.title")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("dashboards.column.version")}
            </th>
            <th scope="col" className="py-2.5 pr-6 pl-3 font-medium whitespace-nowrap">
              {t("dashboards.column.updated")}
            </th>
          </tr>
        </thead>
        <tbody>
          {dashboards.map((dashboard) => (
            <tr key={dashboard.dashboard_id} className="border-b last:border-b-0 hover:bg-accent/60">
              <td className="max-w-md px-6 py-3">
                <span className="flex flex-wrap items-center gap-2">
                  <DashboardLink dashboard={dashboard} />
                  {dashboard.archived_at ? <ArchivedBadge /> : null}
                </span>
              </td>
              <td className="px-3 py-3 whitespace-nowrap tabular-nums">
                {t("dashboards.version", { version: dashboard.latest_version })}
              </td>
              <td className="py-3 pr-6 pl-3 whitespace-nowrap tabular-nums">
                <time dateTime={dashboard.updated_at}>{formatDateTime(dashboard.updated_at)}</time>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <ul className="divide-y md:hidden">
        {dashboards.map((dashboard) => (
          <li key={dashboard.dashboard_id} className="grid gap-1 px-4 py-3">
            <span className="flex flex-wrap items-center gap-2">
              <DashboardLink dashboard={dashboard} />
              {dashboard.archived_at ? <ArchivedBadge /> : null}
            </span>
            <span className="text-sm text-muted-foreground tabular-nums">
              {t("dashboards.version", { version: dashboard.latest_version })}
              {" · "}
              <time dateTime={dashboard.updated_at}>
                {t("dashboards.updated_at", { time: formatDateTime(dashboard.updated_at) })}
              </time>
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}
