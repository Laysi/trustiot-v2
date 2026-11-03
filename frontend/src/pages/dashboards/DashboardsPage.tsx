import { useInfiniteQuery } from "@tanstack/react-query";
import { ArchiveIcon, ArchiveRestoreIcon, EyeIcon, LayoutDashboardIcon, SearchIcon, SearchXIcon } from "lucide-react";
import { type ReactNode, useId, useState } from "react";
import { Link, useSearchParams } from "react-router";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import type { DashboardSummary } from "../../api/dashboards.ts";
import { dashboardPath } from "../../app/paths.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { useSetArchived } from "./archive.ts";
import { creatorLabel } from "./creator.ts";
import { PageHeader } from "./PageHeader.tsx";
import { dashboardListKey } from "./queries.ts";

/** `?tab=archived` lists the archived dashboards, so a link or a reload keeps the choice. */
const TAB_PARAM = "tab";
type Tab = "active" | "archived";

/**
 * The tenant's saved dashboards (`/dashboards`, checklist J4), the most recently updated first:
 * "In use" and "Archived" tabs, a search by name, and for each dashboard its page, archiving
 * and unarchiving. The layout follows docs/design/chat-dashboards/list.html. Dashboards are made
 * and changed in a conversation with the assistant, so the page has no "new" button yet
 * (Laysi/trustiot-v2#149 brings the conversation).
 */
export function DashboardsPage() {
  const { t, errorMessage } = useI18n();
  const api = useDashboardApi();
  const { session } = useSession();
  const tenant = session?.activeTenant;
  const [params, setParams] = useSearchParams();
  const tab: Tab = params.get(TAB_PARAM) === "archived" ? "archived" : "active";
  const [query, setQuery] = useState("");
  const [message, setMessage] = useState<{ kind: "ok" | "error"; text: string } | null>(null);
  const searchId = useId();
  const archive = useSetArchived();

  // Archived dashboards come with the others (`include_archived`); the "Archived" tab keeps them.
  const list = useInfiniteQuery({
    queryKey: dashboardListKey(tenant?.id ?? "", tab === "archived"),
    queryFn: ({ pageParam, signal }) =>
      api.listDashboards(
        { includeArchived: tab === "archived", ...(pageParam !== undefined ? { cursor: pageParam } : {}) },
        signal,
      ),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });

  const viewer = tenant?.role === "viewer";
  const search = query.trim().toLowerCase();
  const loaded = list.data?.pages.flatMap((page) => page.dashboards) ?? [];
  const shown = loaded
    .filter((dashboard) => (dashboard.archived_at != null) === (tab === "archived"))
    .filter((dashboard) => search === "" || dashboard.title.toLowerCase().includes(search));

  const setTab = (next: Tab) => {
    setMessage(null);
    setParams(
      (current) => {
        const params = new URLSearchParams(current);
        if (next === "archived") params.set(TAB_PARAM, "archived");
        else params.delete(TAB_PARAM);
        return params;
      },
      { replace: true },
    );
  };

  const toggleArchived = (dashboard: DashboardSummary) => {
    const archived = dashboard.archived_at == null;
    setMessage(null);
    archive.mutate(
      { dashboardId: dashboard.dashboard_id, archived },
      {
        onSuccess: () =>
          setMessage({
            kind: "ok",
            text: t(archived ? "dashboards.message.archived" : "dashboards.message.unarchived", {
              title: dashboard.title,
            }),
          }),
        onError: (error) => setMessage({ kind: "error", text: errorMessage(error) }),
      },
    );
  };

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
  } else if (shown.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
        {search !== "" ? (
          <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        ) : (
          <LayoutDashboardIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        )}
        <p className="font-semibold">
          {search !== ""
            ? t("dashboards.empty.search_title", { query: query.trim() })
            : t(tab === "archived" ? "dashboards.empty.archived_title" : "dashboards.empty.title")}
        </p>
        <p className="text-sm text-muted-foreground">
          {t(
            search !== ""
              ? "dashboards.empty.search_body"
              : tab === "archived"
                ? "dashboards.empty.archived_body"
                : "dashboards.empty.body",
          )}
        </p>
        {list.hasNextPage ? (
          <Button
            variant="outline"
            size="sm"
            onClick={() => void list.fetchNextPage()}
            disabled={list.isFetchingNextPage}
          >
            {list.isFetchingNextPage ? t("dashboards.loading_more") : t("dashboards.load_more")}
          </Button>
        ) : null}
      </div>
    );
  } else {
    body = (
      <DashboardTable
        dashboards={shown}
        tab={tab}
        canChange={!viewer}
        me={session?.user.id}
        pending={archive.isPending ? archive.variables?.dashboardId : undefined}
        onToggle={toggleArchived}
      />
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={t("dashboards.title")} meta={t("dashboards.description", { tenant: tenant?.name ?? "" })} />

      {viewer ? (
        <Alert role="status" className="grid-cols-[1rem_1fr]">
          <EyeIcon aria-hidden="true" />
          <AlertTitle>{t("dashboards.viewer.title")}</AlertTitle>
          <AlertDescription>
            <p>{t("dashboards.viewer.body")}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      <div className="flex flex-wrap items-center justify-between gap-3">
        <fieldset className="inline-flex min-w-0 gap-0.5 rounded-md bg-muted p-[3px]">
          <legend className="sr-only">{t("dashboards.tabs_label")}</legend>
          {(["active", "archived"] as const).map((value) => (
            <Button
              key={value}
              size="sm"
              variant="ghost"
              aria-pressed={tab === value}
              className={cn(
                "h-[30px] px-3 text-muted-foreground hover:bg-transparent",
                tab === value && "bg-background text-foreground shadow-sm hover:bg-background",
              )}
              onClick={() => setTab(value)}
            >
              {t(value === "active" ? "dashboards.tab.active" : "dashboards.tab.archived")}
            </Button>
          ))}
        </fieldset>
        <div className="relative w-full sm:w-72">
          <label htmlFor={searchId} className="sr-only">
            {t("dashboards.search.label")}
          </label>
          <SearchIcon
            aria-hidden="true"
            className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
          />
          <input
            id={searchId}
            type="search"
            value={query}
            placeholder={t("dashboards.search.placeholder")}
            className={cn(controlClasses, "pl-9")}
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
      </div>

      {body}

      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 text-xs text-muted-foreground">
        {/* Writes say what they did here, beside the list, and are read out. */}
        <p role="status" className={cn("min-h-4", message?.kind === "error" ? "text-destructive" : "text-foreground")}>
          {message?.text}
        </p>
        {list.data !== undefined && list.data !== null ? (
          <p>{t(list.hasNextPage ? "dashboards.footer_more" : "dashboards.footer", { count: shown.length })}</p>
        ) : null}
      </div>
      {list.hasNextPage && shown.length > 0 ? (
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

/** Rows that stand in for the list while it loads; announced once, not row by row. */
function ListSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5 sm:px-6">
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

/** One row's actions: open, and for who may change dashboards, archive or unarchive. */
function RowActions(props: {
  dashboard: DashboardSummary;
  canChange: boolean;
  pending: boolean;
  onToggle(dashboard: DashboardSummary): void;
}) {
  const { t } = useI18n();
  const { dashboard } = props;
  const archived = dashboard.archived_at != null;
  return (
    <div className="flex flex-wrap items-center justify-end gap-1">
      <Button asChild variant="outline" size="sm">
        <Link
          to={dashboardPath(dashboard.dashboard_id)}
          aria-label={t("dashboards.open_named", { title: dashboard.title })}
        >
          {t("dashboards.open")}
        </Link>
      </Button>
      {props.canChange ? (
        <Button
          variant="ghost"
          size="sm"
          disabled={props.pending}
          aria-label={t(archived ? "dashboards.unarchive_named" : "dashboards.archive_named", {
            title: dashboard.title,
          })}
          onClick={() => props.onToggle(dashboard)}
        >
          {archived ? <ArchiveRestoreIcon /> : <ArchiveIcon />}
          {t(archived ? "dashboards.unarchive" : "dashboards.archive")}
        </Button>
      ) : null}
    </div>
  );
}

/**
 * The dashboards as a table, which scrolls sideways in its frame on a narrow screen
 * (docs/design/chat-dashboards/list.html keeps the same columns at every width).
 */
function DashboardTable(props: {
  dashboards: readonly DashboardSummary[];
  tab: Tab;
  canChange: boolean;
  me: string | undefined;
  pending: string | undefined;
  onToggle(dashboard: DashboardSummary): void;
}) {
  const { t, formatDateTime } = useI18n();
  return (
    <section
      aria-label={t("dashboards.list_label")}
      className="w-full overflow-x-auto rounded-lg border bg-card text-card-foreground"
    >
      <table className="w-full min-w-[42rem] text-sm">
        <caption className="sr-only">
          {t(props.tab === "archived" ? "dashboards.caption_archived" : "dashboards.caption_active", {
            count: props.dashboards.length,
          })}
        </caption>
        <thead className="bg-muted/60 text-left text-muted-foreground">
          <tr>
            <th scope="col" className="px-4 py-2.5 font-medium">
              {t("dashboards.column.title")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("dashboards.column.updated")}
            </th>
            <th scope="col" className="px-3 py-2.5 text-right font-medium whitespace-nowrap">
              {t("dashboards.column.version")}
            </th>
            <th scope="col" className="px-4 py-2.5">
              <span className="sr-only">{t("dashboards.column.actions")}</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {props.dashboards.map((dashboard) => (
            <tr key={dashboard.dashboard_id} className="border-t">
              <th scope="row" className="min-w-64 px-4 py-3 text-left font-normal">
                <span className="grid justify-items-start gap-1">
                  <Link
                    to={dashboardPath(dashboard.dashboard_id)}
                    className={cn("rounded-sm font-medium wrap-anywhere underline-offset-4 hover:underline", focusRing)}
                  >
                    {dashboard.title}
                  </Link>
                  {dashboard.archived_at ? (
                    <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
                      <ArchiveIcon aria-hidden="true" className="size-3.5" />
                      {t("dashboards.archived_at", { time: formatDateTime(dashboard.archived_at) })}
                    </span>
                  ) : null}
                </span>
              </th>
              <td className="px-3 py-3 whitespace-nowrap">
                <div>{creatorLabel(dashboard.created_by, props.me, t)}</div>
                <time dateTime={dashboard.updated_at} className="text-xs text-muted-foreground tabular-nums">
                  {formatDateTime(dashboard.updated_at)}
                </time>
              </td>
              <td className="px-3 py-3 text-right whitespace-nowrap tabular-nums">
                {t("dashboards.version", { version: dashboard.latest_version })}
              </td>
              <td className="px-4 py-3">
                <RowActions
                  dashboard={dashboard}
                  canChange={props.canChange}
                  pending={props.pending === dashboard.dashboard_id}
                  onToggle={props.onToggle}
                />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}
