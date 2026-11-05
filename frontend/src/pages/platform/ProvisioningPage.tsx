import { useInfiniteQuery } from "@tanstack/react-query";
import { FactoryIcon } from "lucide-react";
import { type ReactNode, useId, useState } from "react";

import { usePlatformApi } from "../../api/pending/PlatformApiProvider.tsx";
import type { ProvisioningEntry } from "../../api/pending/platform.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { provisioningKey, useUserNames } from "./queries.ts";
import { Skeleton } from "./ui/skeleton.tsx";
import { StackTable, Td, Th, Tr } from "./ui/stack-table.tsx";

function instant(value: string): string | undefined {
  if (value === "") return undefined;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? undefined : date.toISOString();
}

/**
 * The provisioning events (`/platform/provisioning`, docs/design/platform-admin/provisioning.html):
 * the factory registrations that the production workstations make, newest first, read only here.
 * A filter by workstation and a time range (`from` before `to`), a page at a time by cursor.
 */
export function ProvisioningPage() {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = usePlatformApi();
  const [workstation, setWorkstation] = useState("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const workstationId = useId();
  const fromId = useId();
  const toId = useId();
  const fromInstant = instant(from);
  const toInstant = instant(to);
  const rangeInvalid = fromInstant !== undefined && toInstant !== undefined && fromInstant >= toInstant;

  const list = useInfiniteQuery({
    queryKey: provisioningKey(workstation, fromInstant ?? "", toInstant ?? ""),
    queryFn: ({ pageParam, signal }) =>
      api.listProvisioning(
        {
          ...(workstation ? { workstation } : {}),
          ...(fromInstant ? { from: fromInstant } : {}),
          ...(toInstant ? { to: toInstant } : {}),
          ...(pageParam !== undefined ? { cursor: pageParam } : {}),
        },
        signal,
      ),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.nextCursor ?? undefined,
    enabled: !rangeInvalid,
  });
  const entries = list.data?.pages.flatMap((page) => page.entries) ?? [];
  const workstations = list.data?.pages[0]?.workstations ?? [];
  const nameOf = useUserNames(entries.map((entry) => entry.actorUserId));

  let body: ReactNode = null;
  if (rangeInvalid) {
    body = null;
  } else if (list.isPending) {
    body = (
      <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5">
        <p role="status" className="sr-only">
          {t("platform.provisioning.loading")}
        </p>
        {[0, 1, 2].map((row) => (
          <div key={row} className="flex items-center justify-between gap-4">
            <Skeleton className="w-1/6" />
            <Skeleton className="w-1/5" />
            <Skeleton className="w-1/6" />
          </div>
        ))}
      </section>
    );
  } else if (list.data === undefined) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p className="font-medium">{t("platform.provisioning.load_failed")}</p>
          <p>{errorMessage(list.error)}</p>
          <p>{t("platform.provisioning.load_failed_note")}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (entries.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
        <FactoryIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <h2 className="font-semibold">{t("platform.provisioning.empty_title")}</h2>
        <p className="text-sm text-muted-foreground">{t("platform.provisioning.empty_body")}</p>
      </div>
    );
  } else {
    body = (
      <>
        <StackTable
          caption={t("platform.provisioning.caption", { count: entries.length })}
          head={
            <>
              <Th>{t("platform.provisioning.column.time")}</Th>
              <Th>{t("platform.provisioning.column.id")}</Th>
              <Th>{t("platform.provisioning.column.workstation")}</Th>
              <Th>{t("platform.provisioning.column.actor")}</Th>
              <Th>{t("platform.provisioning.column.now")}</Th>
            </>
          }
        >
          {entries.map((entry) => (
            <Tr key={entry.eventId}>
              <Td label={t("platform.provisioning.column.time")} className="sm:whitespace-nowrap">
                <time dateTime={entry.occurredAt} className="tabular-nums">
                  {formatDateTime(entry.occurredAt)}
                </time>
              </Td>
              <Td label={t("platform.provisioning.column.id")} className="font-mono">
                {entry.hardwareId}
              </Td>
              <Td label={t("platform.provisioning.column.workstation")}>{entry.workstation}</Td>
              <Td label={t("platform.provisioning.column.actor")}>
                {entry.actorUserId ? nameOf(entry.actorUserId) : "—"}
              </Td>
              <Td label={t("platform.provisioning.column.now")}>
                <Placement entry={entry} />
              </Td>
            </Tr>
          ))}
        </StackTable>
        <div className="flex flex-wrap items-center justify-between gap-3 text-xs text-muted-foreground">
          <p>{t("platform.provisioning.footer")}</p>
          {list.hasNextPage ? (
            <Button variant="outline" onClick={() => void list.fetchNextPage()} disabled={list.isFetchingNextPage}>
              {list.isFetchingNextPage ? t("platform.audit.loading_more") : t("platform.audit.load_more")}
            </Button>
          ) : null}
        </div>
      </>
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={t("platform.provisioning.title")} meta={t("platform.provisioning.description")} />
      <search aria-label={t("platform.provisioning.filters")} className="flex flex-wrap items-end gap-3">
        <div className="grid w-48 gap-1.5">
          <label htmlFor={workstationId} className="text-sm font-medium">
            {t("platform.provisioning.workstation")}
          </label>
          <NativeSelect id={workstationId} value={workstation} onChange={(event) => setWorkstation(event.target.value)}>
            <option value="">{t("platform.provisioning.workstation_all")}</option>
            {workstations.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </NativeSelect>
        </div>
        <div className="grid gap-1.5">
          <label htmlFor={fromId} className="text-sm font-medium">
            {t("platform.audit.from")}
          </label>
          <input
            id={fromId}
            type="datetime-local"
            value={from}
            aria-invalid={rangeInvalid ? true : undefined}
            className={cn(controlClasses, "w-48")}
            onChange={(event) => setFrom(event.target.value)}
          />
        </div>
        <div className="grid gap-1.5">
          <label htmlFor={toId} className="text-sm font-medium">
            {t("platform.audit.to")}
          </label>
          <input
            id={toId}
            type="datetime-local"
            value={to}
            aria-invalid={rangeInvalid ? true : undefined}
            className={cn(controlClasses, "w-48")}
            onChange={(event) => setTo(event.target.value)}
          />
        </div>
      </search>
      {rangeInvalid ? (
        <p role="alert" className="text-sm font-medium text-destructive">
          {t("platform.audit.range_invalid")}
        </p>
      ) : null}
      {body}
    </div>
  );
}

function Placement({ entry }: { entry: ProvisioningEntry }) {
  const { t } = useI18n();
  if (entry.placement === "bound") return <>{entry.tenantName ?? "—"}</>;
  return <>{t(PLACEMENT_LABELS[entry.placement])}</>;
}

const PLACEMENT_LABELS = {
  pool: "platform.provisioning.placement.pool",
  bound: "platform.provisioning.placement.bound",
  repair: "platform.provisioning.placement.repair",
  decommissioned: "platform.provisioning.placement.decommissioned",
} as const;
