import { useInfiniteQuery } from "@tanstack/react-query";
import { XIcon } from "lucide-react";
import { useEffect, useId, useRef } from "react";
import { Link } from "react-router";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import type { VersionSummary } from "../../api/dashboards.ts";
import { dashboardPath } from "../../app/paths.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { creatorLabel } from "./creator.ts";
import { versionListKey } from "./queries.ts";
import { RestoreAction } from "./RestoreAction.tsx";
import { Tag } from "./Tag.tsx";

function originText(version: VersionSummary, t: ReturnType<typeof useI18n>["t"]): string {
  switch (version.origin) {
    case "agent":
      return t("dashboard_page.origin.agent");
    case "publish":
      return t("dashboard_page.origin.publish");
    case "restore":
      return version.restored_from != null
        ? t("dashboard_page.origin.restore_from", { version: version.restored_from })
        : t("dashboard_page.origin.restore");
  }
}

/**
 * The version history (docs/design/chat-dashboards/design.md, 「版本紀錄」): a dashboard's
 * versions, the newest first (`GET …/versions`, 50 to a page). Each row says what the version is
 * (the latest, the one shown), how it came to be and who made it; an older version can be viewed
 * (its own address, `?version=`) and, for who may restore, restored in place. The list scrolls
 * inside its frame, so a long history does not push the dashboard out of the screen.
 */
export function VersionsPanel(props: {
  id: string;
  dashboardId: string;
  shown: number;
  latest: number;
  /** Whether this user may restore a version here: they may, and the dashboard is not archived. */
  canRestore: boolean;
  onRestored(created: { from: number; version: number }): void;
  onClose(): void;
}) {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = useDashboardApi();
  const { session } = useSession();
  const tenant = session?.activeTenant?.id ?? "";
  const headingId = useId();
  const heading = useRef<HTMLHeadingElement>(null);

  const versions = useInfiniteQuery({
    queryKey: versionListKey(tenant, props.dashboardId),
    queryFn: ({ pageParam, signal }) => api.listVersions(props.dashboardId, pageParam, signal),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });

  // Focus moves to the panel when it opens, so a keyboard user lands on what they opened.
  useEffect(() => {
    heading.current?.focus();
  }, []);

  const list = versions.data?.pages.flatMap((page) => page.versions) ?? [];
  return (
    <section
      id={props.id}
      aria-labelledby={headingId}
      className="grid gap-3 rounded-xl border bg-card px-4 py-4 text-card-foreground shadow-sm sm:px-5"
    >
      <div className="flex items-center justify-between gap-2">
        <h2 id={headingId} ref={heading} tabIndex={-1} className="text-base font-semibold outline-none">
          {t("dashboard_page.versions")}
        </h2>
        <Button variant="ghost" size="icon" aria-label={t("dashboard_page.versions_close")} onClick={props.onClose}>
          <XIcon />
        </Button>
      </div>
      {versions.isPending ? (
        <Spinner label={t("dashboard_page.versions_loading")} />
      ) : versions.isError ? (
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{t("dashboard_page.versions_failed", { reason: errorMessage(versions.error) })}</p>
          </AlertDescription>
          <Button variant="outline" size="sm" onClick={() => void versions.refetch()}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      ) : (
        <ol className="max-h-96 divide-y overflow-y-auto rounded-lg border text-sm">
          {list.map((version) => {
            const current = version.version === props.shown;
            const latest = version.version === props.latest;
            return (
              <li
                key={version.version}
                aria-current={current ? "true" : undefined}
                className={cn(
                  "flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-3 py-2.5",
                  current && "bg-brand-soft",
                )}
              >
                <span className="grid min-w-0 gap-0.5">
                  <span className="flex flex-wrap items-center gap-2 font-medium">
                    {t("dashboards.version", { version: version.version })}
                    {latest ? <Tag>{t("dashboard_page.latest")}</Tag> : null}
                    {current ? <Tag>{t("dashboard_page.shown")}</Tag> : null}
                  </span>
                  <span className="text-muted-foreground wrap-anywhere">
                    {originText(version, t)} · {creatorLabel(version.created_by, session?.user.id, t)} ·{" "}
                    <time dateTime={version.created_at} className="tabular-nums">
                      {formatDateTime(version.created_at)}
                    </time>
                  </span>
                  {version.title ? <span className="text-muted-foreground wrap-anywhere">{version.title}</span> : null}
                </span>
                <span className="flex flex-wrap items-center gap-2">
                  {current ? null : (
                    <Button asChild variant="outline" size="sm">
                      <Link
                        to={dashboardPath(props.dashboardId, latest ? undefined : version.version)}
                        aria-label={t("dashboard_page.view_version", { version: version.version })}
                      >
                        {t("dashboard_page.view")}
                      </Link>
                    </Button>
                  )}
                  {props.canRestore && !latest ? (
                    <RestoreAction
                      dashboardId={props.dashboardId}
                      version={version.version}
                      latest={props.latest}
                      variant="outline"
                      onRestored={props.onRestored}
                    />
                  ) : null}
                </span>
              </li>
            );
          })}
        </ol>
      )}
      {versions.hasNextPage ? (
        <div>
          <Button
            variant="outline"
            size="sm"
            onClick={() => void versions.fetchNextPage()}
            disabled={versions.isFetchingNextPage}
          >
            {versions.isFetchingNextPage ? t("dashboards.loading_more") : t("dashboards.load_more")}
          </Button>
        </div>
      ) : null}
    </section>
  );
}
