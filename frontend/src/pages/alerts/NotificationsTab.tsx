import { useInfiniteQuery } from "@tanstack/react-query";
import { BellIcon, CheckCheckIcon, EllipsisIcon, InfoIcon } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router";

import { useAlertsApi } from "../../api/pending/AlertsApiProvider.tsx";
import type { FeedItem, LimitNotice, Notification } from "../../api/pending/alerts.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "../../components/ui/dropdown-menu.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { EmptyState } from "../../components/EmptyState.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useMarkAllRead, useMarkRead } from "./actions.ts";
import { notificationsKey } from "./queries.ts";
import { rulePath } from "./paths.ts";
import { AskAiButton, ListSkeleton, OriginBadge, Segmented } from "./parts.tsx";
import { useAlertsRole, useTenantId } from "./state.ts";
import { useAlertsTime } from "./time.ts";

const SHOW_PARAM = "show";
const DEVICE_PARAM = "device";
const RULE_PARAM = "rule";

/** How often an open list looks for new notifications; the live push (SSE) comes with the backend. */
const REFRESH_MS = 30_000;

/**
 * The notification list (design 「通知清單」): newest first, unread first by default, each with the
 * sentence the rule wrote, who made the rule, the merge count of the dedup, and its actions. A
 * rule at its hourly limit leaves a note in the list, so a quiet rule is not mistaken for a broken
 * one. Opening a notification marks it read (per person, decision Q4) and leads to the rule's
 * recent triggers.
 */
export function NotificationsTab({
  unreadCount,
  highlight,
  onDisable,
  onError,
}: {
  unreadCount: number;
  /** A notification a deep link names: shown first when it is not in the loaded pages, and marked. */
  highlight?: Notification | undefined;
  onDisable(rule: { id: string; name: string }): void;
  onError(error: unknown): void;
}) {
  const { t, errorMessage, formatNumber } = useI18n();
  const api = useAlertsApi();
  const tenant = useTenantId();
  const { canManage } = useAlertsRole();
  const [params, setParams] = useSearchParams();
  // A deep link to one notification shows everything, so the one it names is in the list.
  const showAll = params.get(SHOW_PARAM) === "all" || (highlight !== undefined && params.get(SHOW_PARAM) !== "unread");
  const deviceId = params.get(DEVICE_PARAM) ?? undefined;
  const ruleId = params.get(RULE_PARAM) ?? undefined;
  const filters = { unreadOnly: !showAll, ...(deviceId ? { deviceId } : {}), ...(ruleId ? { ruleId } : {}) };
  const markAll = useMarkAllRead();
  const markRead = useMarkRead();

  const list = useInfiniteQuery({
    queryKey: notificationsKey(tenant, filters),
    queryFn: ({ pageParam, signal }) =>
      api.listNotifications({ ...filters, ...(pageParam !== undefined ? { cursor: pageParam } : {}) }, signal),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor,
    refetchInterval: REFRESH_MS,
  });

  const setParam = (name: string, value: string) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value === "") next.delete(name);
        else next.set(name, value);
        return next;
      },
      { replace: true },
    );

  const loaded = list.data?.pages.flatMap((page) => page.items) ?? [];
  const items: FeedItem[] =
    highlight !== undefined && !loaded.some((item) => item.id === highlight.id) ? [highlight, ...loaded] : loaded;
  const first = list.data?.pages[0];
  const announcement = useNewNotificationAnnouncement(items);

  let body: ReactNode;
  if (list.isPending) {
    body = <ListSkeleton label={t("alerts.notifications.loading")} />;
  } else if (list.isError) {
    body = <LoadFailed message={errorMessage(list.error)} onRetry={() => void list.refetch()} busy={list.isFetching} />;
  } else if (items.length === 0) {
    body = (
      <NothingToShow
        showAll={showAll}
        filtered={deviceId !== undefined || ruleId !== undefined}
        onShowAll={() => setParam(SHOW_PARAM, "all")}
        onClearFilters={() =>
          setParams(
            (current) => {
              const next = new URLSearchParams(current);
              next.delete(DEVICE_PARAM);
              next.delete(RULE_PARAM);
              return next;
            },
            { replace: true },
          )
        }
      />
    );
  } else {
    body = (
      <ul aria-label={t("alerts.notifications.list_label")} className="grid divide-y rounded-lg border bg-card">
        {items.map((item) =>
          item.type === "limit_reached" ? (
            <LimitRow key={item.id} notice={item} />
          ) : (
            <NotificationRow
              key={item.id}
              notification={item}
              highlighted={item.id === highlight?.id}
              canManage={canManage}
              onOpen={() => {
                if (!item.read) markRead.mutate(item.id, { onError });
              }}
              onMarkRead={() => markRead.mutate(item.id, { onError })}
              onDisable={() => onDisable({ id: item.rule_id, name: item.rule_name })}
            />
          ),
        )}
      </ul>
    );
  }

  return (
    <div className="grid gap-4">
      <div className="flex flex-wrap items-center gap-3">
        <Segmented
          label={t("alerts.notifications.show")}
          value={showAll ? "all" : "unread"}
          onChange={(value) => setParam(SHOW_PARAM, value === "all" ? "all" : "unread")}
          options={[
            { id: "unread", label: t("alerts.notifications.show_unread") },
            { id: "all", label: t("alerts.notifications.show_all") },
          ]}
        />
        <div className="w-full sm:w-52">
          <label htmlFor="alerts-filter-device" className="sr-only">
            {t("alerts.notifications.filter_device")}
          </label>
          <NativeSelect
            id="alerts-filter-device"
            value={deviceId ?? ""}
            onChange={(event) => setParam(DEVICE_PARAM, event.target.value)}
          >
            <option value="">{t("alerts.notifications.all_devices")}</option>
            {optionsWith(first?.devices, deviceId).map((device) => (
              <option key={device.id} value={device.id}>
                {device.name}
              </option>
            ))}
          </NativeSelect>
        </div>
        <div className="w-full sm:w-64">
          <label htmlFor="alerts-filter-rule" className="sr-only">
            {t("alerts.notifications.filter_rule")}
          </label>
          <NativeSelect
            id="alerts-filter-rule"
            value={ruleId ?? ""}
            onChange={(event) => setParam(RULE_PARAM, event.target.value)}
          >
            <option value="">{t("alerts.notifications.all_rules")}</option>
            {optionsWith(first?.rules, ruleId).map((rule) => (
              <option key={rule.id} value={rule.id}>
                {rule.name}
              </option>
            ))}
          </NativeSelect>
        </div>
        {unreadCount > 0 ? (
          <Button
            variant="ghost"
            size="sm"
            className="sm:ml-auto"
            disabled={markAll.isPending}
            onClick={() => markAll.mutate(undefined, { onError })}
          >
            <CheckCheckIcon aria-hidden="true" />
            {t("alerts.notifications.mark_all")}
          </Button>
        ) : null}
      </div>

      {body}

      {list.hasNextPage ? (
        <div>
          <Button variant="outline" disabled={list.isFetchingNextPage} onClick={() => void list.fetchNextPage()}>
            {t("alerts.notifications.load_more")}
          </Button>
        </div>
      ) : null}
      {list.isFetchNextPageError ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{errorMessage(list.error)}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      {/* New notifications are read out politely, once, without moving the focus. */}
      <p role="status" aria-live="polite" className="sr-only">
        {announcement === 0 ? "" : t("alerts.notifications.new", { count: formatNumber(announcement) })}
      </p>
    </div>
  );
}

/** The options of a filter, plus the one the address names when the server's list does not have it, so the select says what is filtered. */
function optionsWith(
  options: readonly { id: string; name: string }[] | undefined,
  selected: string | undefined,
): { id: string; name: string }[] {
  const known = [...(options ?? [])];
  if (selected !== undefined && !known.some((option) => option.id === selected)) {
    known.push({ id: selected, name: selected });
  }
  return known;
}

/** How many notifications arrived since the list first showed; `0` before there is something to compare with. */
function useNewNotificationAnnouncement(items: readonly FeedItem[]): number {
  const seen = useRef<Set<string> | null>(null);
  const [fresh, setFresh] = useState(0);
  useEffect(() => {
    const ids = items.filter((item) => item.type === "notification").map((item) => item.id);
    if (seen.current === null) {
      if (ids.length > 0) seen.current = new Set(ids);
      return;
    }
    const arrived = ids.filter((id) => !seen.current?.has(id));
    if (arrived.length > 0) {
      for (const id of arrived) seen.current.add(id);
      setFresh(arrived.length);
    }
  }, [items]);
  return fresh;
}

function LoadFailed({ message, onRetry, busy }: { message: string; onRetry(): void; busy: boolean }) {
  const { t } = useI18n();
  return (
    <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
      <AlertDescription className="col-start-1">
        <p className="font-medium">{t("alerts.error.title")}</p>
        <p>{t("alerts.error.body")}</p>
        <p className="text-xs">{message}</p>
      </AlertDescription>
      <Button variant="outline" onClick={onRetry} disabled={busy}>
        {t("page.action.retry")}
      </Button>
    </Alert>
  );
}

function NothingToShow({
  showAll,
  filtered,
  onShowAll,
  onClearFilters,
}: {
  showAll: boolean;
  filtered: boolean;
  onShowAll(): void;
  onClearFilters(): void;
}) {
  const { t } = useI18n();
  if (filtered) {
    return (
      <EmptyState icon={<BellIcon />} title={t("alerts.notifications.no_match.title")}
        action={
          <Button variant="outline" onClick={onClearFilters}>
            {t("alerts.notifications.clear_filters")}
          </Button>
        }
      >
        <p>{t("alerts.notifications.no_match.body")}</p>
      </EmptyState>
    );
  }
  if (!showAll) {
    return (
      <EmptyState
        icon={<BellIcon />}
        title={t("alerts.notifications.none_unread.title")}
        action={
          <Button variant="outline" onClick={onShowAll}>
            {t("alerts.notifications.show_all_action")}
          </Button>
        }
      >
        <p>{t("alerts.notifications.none_unread.body")}</p>
      </EmptyState>
    );
  }
  return (
    <EmptyState
      icon={<BellIcon />}
      title={t("alerts.notifications.empty.title")}
      action={<AskAiButton question={t("alerts.ask.suggest_rules")} context={[t("alerts.ask.context_all_devices"), t("alerts.ask.context_14_days")]} />}
    >
      <p>{t("alerts.notifications.empty.body")}</p>
    </EmptyState>
  );
}

function LimitRow({ notice }: { notice: LimitNotice }) {
  const { t } = useI18n();
  const time = useAlertsTime();
  return (
    <li role="note" className="grid grid-cols-[1.25rem_1fr] items-start gap-x-3 bg-muted px-4 py-3 text-sm">
      <InfoIcon aria-hidden="true" className="mt-0.5 size-4 text-muted-foreground" />
      <div className="grid gap-1">
        <p>
          {t("alerts.notifications.limit", {
            rule: notice.rule_name,
            count: notice.count,
            until: time.when(notice.until),
          })}
        </p>
        <p className="text-xs text-muted-foreground">
          <time dateTime={notice.at}>{time.when(notice.at)}</time>
        </p>
      </div>
    </li>
  );
}

function NotificationRow({
  notification,
  highlighted,
  canManage,
  onOpen,
  onMarkRead,
  onDisable,
}: {
  notification: Notification;
  highlighted: boolean;
  canManage: boolean;
  onOpen(): void;
  onMarkRead(): void;
  onDisable(): void;
}) {
  const { t } = useI18n();
  const time = useAlertsTime();
  const row = useRef<HTMLLIElement>(null);
  useEffect(() => {
    if (highlighted) row.current?.scrollIntoView?.({ block: "center" });
  }, [highlighted]);
  const question = t(notification.kind === "offline" ? "alerts.ask.why_offline" : "alerts.ask.why_triggered");
  const context = [
    t("alerts.ask.context_rule", { rule: notification.rule_name }),
    t("alerts.ask.context_device", { device: notification.device_name }),
    t("alerts.ask.context_around", { time: time.full(notification.triggered_at) }),
  ];
  const showDisable = canManage && !notification.rule_disabled;
  return (
    <li
      ref={row}
      aria-current={highlighted ? "true" : undefined}
      className={cn(
        "grid grid-cols-[1rem_minmax(0,1fr)] items-start gap-x-3 gap-y-2 px-4 py-3 sm:grid-cols-[1rem_minmax(0,1fr)_auto]",
        highlighted && "outline-2 -outline-offset-2 outline-ring",
      )}
    >
      <span className="mt-2 flex size-2 items-center justify-center">
        {notification.read ? null : (
          <>
            <span aria-hidden="true" className="size-2 rounded-full bg-primary" />
            <span className="sr-only">{t("alerts.notifications.unread")}</span>
          </>
        )}
      </span>
      <div className="grid min-w-0 gap-1.5">
        <Link
          to={rulePath(notification.rule_id, { section: "recent" })}
          onClick={onOpen}
          className={cn("rounded-sm font-medium wrap-anywhere underline-offset-4 hover:underline", focusRing)}
        >
          {notification.summary}
        </Link>
        <p className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
          <OriginBadge origin={notification.rule_origin} />
          <Link
            to={rulePath(notification.rule_id)}
            className={cn("rounded-sm underline underline-offset-4 hover:text-foreground", focusRing)}
          >
            {notification.rule_name}
          </Link>
          <span>{notification.device_name}</span>
          {notification.part_number ? (
            <span>{t("alerts.notifications.part", { part: notification.part_number })}</span>
          ) : null}
          {notification.rule_disabled ? (
            <span className="rounded-sm bg-muted px-1.5 py-0.5 font-medium text-foreground">
              {t("alerts.notifications.rule_disabled")}
            </span>
          ) : null}
          <time dateTime={notification.triggered_at}>{time.when(notification.triggered_at)}</time>
          {notification.merged_count > 1 ? (
            <span>{t("alerts.notifications.merged", { count: notification.merged_count })}</span>
          ) : null}
        </p>
      </div>
      <div className="col-start-2 flex flex-wrap items-center gap-2 sm:col-start-3 sm:row-start-1">
        <AskAiButton question={question} context={context} size="sm" />
        {showDisable ? (
          <Button variant="outline" size="sm" onClick={onDisable}>
            {t("alerts.notifications.disable")}
          </Button>
        ) : null}
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              variant="ghost"
              size="icon"
              className="size-8"
              aria-label={t("alerts.notifications.more", { rule: notification.rule_name })}
            >
              <EllipsisIcon aria-hidden="true" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem asChild>
              <Link to={rulePath(notification.rule_id)}>{t("alerts.notifications.view_rule")}</Link>
            </DropdownMenuItem>
            {notification.read ? null : (
              <DropdownMenuItem onSelect={onMarkRead}>{t("alerts.notifications.mark_read")}</DropdownMenuItem>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    </li>
  );
}

