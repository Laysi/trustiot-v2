import { useInfiniteQuery } from "@tanstack/react-query";
import { ChevronDownIcon, ChevronRightIcon, CopyIcon, ScrollTextIcon } from "lucide-react";
import { Fragment, useEffect, useState } from "react";

import { usePlatformApi } from "../../../api/pending/PlatformApiProvider.tsx";
import type { EventScope, LoggedEvent } from "../../../api/pending/platform.ts";
import { Alert, AlertDescription } from "../../../components/ui/alert.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { actorUserId, eventsKey, shortId, useUserNames } from "../queries.ts";
import { Badge } from "../ui/badge.tsx";
import { Skeleton } from "../ui/skeleton.tsx";
import { StackTable, Td, Th, Tr } from "../ui/stack-table.tsx";
import { describeEvent, subjectUserId } from "./eventSummary.ts";

/** The groups the "this page only" filter offers, by the part of the type they match. */
export type EventKind = "all" | "state" | "members" | "quota" | "gateway";
export const EVENT_KINDS: readonly EventKind[] = ["all", "state", "members", "quota", "gateway"];

export function matchesKind(type: string, kind: EventKind): boolean {
  if (kind === "all") return true;
  if (kind === "state") return /^(identity\.tenant_(created|state_changed)|certs\.tenant_state_applied)$/.test(type);
  if (kind === "members") return type === "identity.member_role_changed";
  if (kind === "quota") return type === "identity.quota_changed" || type === "identity.tenant_entitlements_changed";
  return /^(assets|certs)\.gateway_/.test(type);
}

/** The events the page loaded so far, for the export. */
export type LoadedEvents = readonly LoggedEvent[];

/**
 * The operation log of one scope as a table, newest first, a page at a time by cursor
 * (docs/design/platform-admin/audit-log.html): the time, the event's name with its type code, the
 * actor and a one-line summary; a row opens to the whole event, with its payload as the producer
 * wrote it. A type this build does not know is listed with its code. `kind` filters only the rows
 * that are loaded (decision Q13: the server takes no type parameter yet). `limit` shows only the
 * first rows and no load-more control, for the card on a tenant's page.
 */
export function AuditEvents(props: {
  scope: EventScope;
  from?: string;
  to?: string;
  kind?: EventKind;
  limit?: number;
  onLoaded?(events: LoadedEvents): void;
}) {
  const { t, errorMessage } = useI18n();
  const api = usePlatformApi();
  const scopeKey = props.scope.kind === "tenant" ? props.scope.tenantId : "platform";
  const list = useInfiniteQuery({
    queryKey: eventsKey(scopeKey, props.from ?? "", props.to ?? ""),
    queryFn: ({ pageParam, signal }) =>
      api.listEvents(
        {
          scope: props.scope,
          ...(props.from ? { from: props.from } : {}),
          ...(props.to ? { to: props.to } : {}),
          ...(pageParam !== undefined ? { cursor: pageParam } : {}),
        },
        signal,
      ),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.nextCursor ?? undefined,
  });
  const loaded = list.data?.pages.flatMap((page) => page.events) ?? [];
  const { onLoaded } = props;
  // biome-ignore lint/correctness/useExhaustiveDependencies: the callback is read when the data changes, not when its identity does
  useEffect(() => {
    onLoaded?.(list.data?.pages.flatMap((page) => page.events) ?? []);
  }, [list.data]);
  const kind = props.kind ?? "all";
  const filtered = loaded.filter((event) => matchesKind(event.type, kind));
  const shown = props.limit === undefined ? filtered : filtered.slice(0, props.limit);
  const nameOf = useUserNames(shown.flatMap((event) => [actorUserId(event.actor), subjectUserId(event)]));

  if (list.isPending) {
    return (
      <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5">
        <p role="status" className="sr-only">
          {t("platform.audit.loading")}
        </p>
        {[0, 1, 2].map((row) => (
          <div key={row} className="flex items-center justify-between gap-4">
            <Skeleton className="w-1/6" />
            <Skeleton className="w-2/5" />
            <Skeleton className="w-1/6" />
          </div>
        ))}
      </section>
    );
  }
  if (list.data === undefined) {
    return (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p className="font-medium">{t("platform.audit.load_failed")}</p>
          <p>{errorMessage(list.error)}</p>
          <p>{t("platform.audit.load_failed_note")}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }
  if (shown.length === 0 && !list.hasNextPage) {
    return (
      <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-10 text-center">
        <ScrollTextIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <p className="font-semibold">
          {t(kind === "all" ? "platform.audit.empty_title" : "platform.audit.empty_kind_title")}
        </p>
        <p className="text-sm text-muted-foreground">
          {t(kind === "all" ? "platform.audit.empty_body" : "platform.audit.empty_kind_body")}
        </p>
      </div>
    );
  }
  return (
    <div className="grid gap-3">
      <EventTable events={shown} nameOf={nameOf} />
      {props.limit === undefined ? (
        <div className="flex flex-wrap items-center justify-between gap-3 text-xs text-muted-foreground">
          <p>{t("platform.audit.footer", { count: loaded.length })}</p>
          {list.hasNextPage ? (
            <Button variant="outline" onClick={() => void list.fetchNextPage()} disabled={list.isFetchingNextPage}>
              {list.isFetchingNextPage ? t("platform.audit.loading_more") : t("platform.audit.load_more")}
            </Button>
          ) : null}
        </div>
      ) : null}
      {list.isFetchNextPageError ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{errorMessage(list.error)}</p>
          </AlertDescription>
        </Alert>
      ) : null}
    </div>
  );
}

function EventTable({ events, nameOf }: { events: readonly LoggedEvent[]; nameOf: (userId: string) => string }) {
  const { t, formatDateTime } = useI18n();
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const toggle = (id: string) =>
    setOpen((current) => {
      const next = new Set(current);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  return (
    <StackTable
      caption={t("platform.audit.caption", { count: events.length })}
      head={
        <>
          <Th>{t("platform.audit.column.time")}</Th>
          <Th>{t("platform.audit.column.event")}</Th>
          <Th>{t("platform.audit.column.actor")}</Th>
          <Th>{t("platform.audit.column.summary")}</Th>
          <Th>
            <span className="sr-only">{t("platform.audit.column.expand")}</span>
          </Th>
        </>
      }
    >
      {events.map((event) => {
        const description = describeEvent(event, t, nameOf);
        const expanded = open.has(event.eventId);
        return (
          <Fragment key={event.eventId}>
            <Tr>
              <Td label={t("platform.audit.column.time")} className="sm:whitespace-nowrap">
                <time dateTime={event.occurredAt} className="tabular-nums">
                  {formatDateTime(event.occurredAt)}
                </time>
              </Td>
              <Td label={t("platform.audit.column.event")}>
                <span className="grid justify-items-start gap-0.5">
                  <span className="font-medium">{description.title}</span>
                  {description.known ? (
                    <span className="font-mono text-xs text-muted-foreground">{event.type}</span>
                  ) : (
                    <Badge tone="outline">{t("platform.audit.unknown_badge")}</Badge>
                  )}
                </span>
              </Td>
              <Td label={t("platform.audit.column.actor")}>
                <Actor event={event} nameOf={nameOf} />
              </Td>
              <Td label={t("platform.audit.column.summary")}>{description.summary}</Td>
              <Td label={t("platform.audit.column.expand")} className="sm:text-right">
                <Button
                  variant="ghost"
                  size="icon"
                  aria-expanded={expanded}
                  aria-label={t(expanded ? "platform.audit.collapse" : "platform.audit.expand", {
                    event: description.title,
                  })}
                  onClick={() => toggle(event.eventId)}
                >
                  {expanded ? <ChevronDownIcon /> : <ChevronRightIcon />}
                </Button>
              </Td>
            </Tr>
            {expanded ? (
              <tr className="block border-t bg-muted/40 p-3 sm:table-row sm:p-0">
                <td colSpan={5} className="block px-4 py-3 sm:table-cell">
                  <EventDetail event={event} nameOf={nameOf} />
                </td>
              </tr>
            ) : null}
          </Fragment>
        );
      })}
    </StackTable>
  );
}

function Actor({ event, nameOf }: { event: LoggedEvent; nameOf: (userId: string) => string }) {
  const { t } = useI18n();
  const actor = event.actor;
  if (actor === null) return <span className="text-muted-foreground">{t("platform.audit.actor_none")}</span>;
  return (
    <span className="grid justify-items-start gap-0.5">
      <span>{actor.kind === "user" ? nameOf(actor.userId) : actor.service}</span>
      <span className="text-xs text-muted-foreground">
        {t(actor.kind === "user" ? "platform.audit.actor_user" : "platform.audit.actor_service")}
      </span>
    </span>
  );
}

function EventDetail({ event, nameOf }: { event: LoggedEvent; nameOf: (userId: string) => string }) {
  const { t, formatDateTime } = useI18n();
  const [copied, setCopied] = useState<"idle" | "done" | "failed">("idle");
  const payload = JSON.stringify(event.payload, null, 2);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(payload);
      setCopied("done");
    } catch {
      setCopied("failed");
    }
  };
  const row = (label: string, value: string) => (
    <div className="grid gap-0.5">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="font-mono text-xs wrap-anywhere">{value}</dd>
    </div>
  );
  return (
    <div className="grid gap-3">
      <dl className="grid gap-3 sm:grid-cols-2">
        {row(t("platform.audit.detail.id"), event.eventId)}
        {row(t("platform.audit.detail.type"), `${event.type} v${event.version}`)}
        {row(t("platform.audit.detail.producer"), event.producer)}
        {row(
          t("platform.audit.detail.aggregate"),
          `${event.aggregateType} ${shortId(event.aggregateId)} #${event.aggregateSeq}`,
        )}
        {row(t("platform.audit.detail.occurred"), formatDateTime(event.occurredAt))}
        {row(t("platform.audit.detail.recorded"), formatDateTime(event.recordedAt))}
        {event.actor?.kind === "user"
          ? row(t("platform.audit.detail.actor"), `${nameOf(event.actor.userId)} (${shortId(event.actor.userId)})`)
          : null}
      </dl>
      <div className="grid gap-1.5">
        <div className="flex items-center justify-between gap-2">
          <p className="text-xs font-medium">{t("platform.audit.detail.payload")}</p>
          <Button variant="outline" size="sm" onClick={() => void copy()}>
            <CopyIcon />
            {t("platform.audit.detail.copy")}
          </Button>
        </div>
        <pre className="max-h-72 overflow-auto rounded-md border bg-background p-3 font-mono text-xs wrap-anywhere whitespace-pre-wrap">
          {payload}
        </pre>
        <p role="status" className="min-h-4 text-xs text-muted-foreground">
          {copied === "done"
            ? t("platform.audit.detail.copied")
            : copied === "failed"
              ? t("platform.audit.detail.copy_failed")
              : ""}
        </p>
      </div>
    </div>
  );
}
