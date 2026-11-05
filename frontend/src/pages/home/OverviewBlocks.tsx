import type { UseQueryResult } from "@tanstack/react-query";
import { CircleCheckIcon, SearchIcon, SearchXIcon, TriangleAlertIcon, UnplugIcon, WifiOffIcon } from "lucide-react";
import { type ReactNode, useId, useState } from "react";
import { Link } from "react-router";

import type {
  DeviceHealthRow,
  DeviceStatus,
  NewAnomalies,
  StatusSummary,
  TopUsage,
} from "../../api/pending/home-overview.ts";
import { DEVICES_PATH, devicePath } from "../../app/paths.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { Card } from "../../components/ui/card.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { useMediaQuery } from "../../lib/use-media-query.ts";
import { cn } from "../../lib/utils.ts";

/**
 * The blocks of the generic home (docs/design/home-ai/10-fallback-generic-home.html). Each takes
 * its own query's result, draws its own loading, empty and error states, and a failure in one
 * never covers the others (the design's 12-E).
 */

const STATUS: Record<DeviceStatus, { label: MessageId; icon: typeof CircleCheckIcon; tone: string }> = {
  normal: { label: "overview.status.normal", icon: CircleCheckIcon, tone: "text-success" },
  abnormal: { label: "overview.status.abnormal", icon: TriangleAlertIcon, tone: "text-warning" },
  offline: { label: "overview.status.offline", icon: WifiOffIcon, tone: "text-muted-foreground" },
};

/** A state in a shape and in words, never in color alone: ✓ normal, ⚠ abnormal, a broken link offline. */
export function StatusBadge({ status }: { status: DeviceStatus }) {
  const { t } = useI18n();
  const { label, icon: Icon, tone } = STATUS[status];
  return (
    <span className={cn("inline-flex items-center gap-1 text-xs font-medium whitespace-nowrap", tone)}>
      <Icon aria-hidden="true" className="size-3.5" />
      {t(label)}
    </span>
  );
}

function BlockSkeleton({ label }: { label: string }) {
  return (
    <div role="status" className="grid gap-2" aria-label={label}>
      <div className="h-5 w-2/3 animate-pulse rounded bg-muted" />
      <div className="h-5 w-1/2 animate-pulse rounded bg-muted" />
      <div className="h-5 w-3/4 animate-pulse rounded bg-muted" />
    </div>
  );
}

/** A block's own failure, with its retry. */
export function BlockError({
  title,
  query,
}: {
  title: string;
  query: Pick<UseQueryResult, "error" | "isFetching" | "refetch">;
}) {
  const { t, errorMessage } = useI18n();
  return (
    <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
      <div className="col-start-1 grid gap-0.5">
        <AlertTitle className="col-start-1">{title}</AlertTitle>
        <AlertDescription className="col-start-1">
          <p>{t("overview.block_failed", { reason: errorMessage(query.error) })}</p>
        </AlertDescription>
      </div>
      <Button variant="outline" size="sm" onClick={() => void query.refetch()} disabled={query.isFetching}>
        {t("page.action.retry")}
      </Button>
    </Alert>
  );
}

// --- status tiles -----------------------------------------------------------------------------

export function StatusTiles({ query }: { query: UseQueryResult<StatusSummary> }) {
  const { t, formatNumber } = useI18n();
  if (query.isPending) return <BlockSkeleton label={t("overview.loading")} />;
  if (query.isError) return <BlockError title={t("overview.status.failed")} query={query} />;
  const summary = query.data;
  const tiles: { status: DeviceStatus; devices: { device_id: string; name: string }[] }[] = [
    { status: "normal", devices: summary.normal },
    { status: "abnormal", devices: summary.abnormal },
    { status: "offline", devices: summary.offline },
  ];
  return (
    <ul className="grid grid-cols-1 gap-3 sm:grid-cols-3">
      {tiles.map(({ status, devices }) => {
        const { label, icon: Icon, tone } = STATUS[status];
        const only = devices.length === 1 ? devices[0] : undefined;
        const detail = only
          ? only.name
          : devices.length === 0
            ? t("overview.tile.none")
            : t(`overview.tile.view_${status}`);
        return (
          <li key={status} className="grid">
            <Link
              to={`${DEVICES_PATH}?status=${status}`}
              className={cn(
                "grid content-start gap-1 rounded-xl border bg-card p-4 text-card-foreground transition-colors hover:border-primary hover:bg-accent",
                focusRing,
              )}
            >
              <span className={cn("flex items-center gap-2 text-sm font-medium", tone)}>
                <Icon aria-hidden="true" className="size-4" />
                {t(label)}
              </span>
              <span className="text-3xl font-semibold tabular-nums">{formatNumber(devices.length)}</span>
              <span className="truncate text-sm text-muted-foreground">{detail}</span>
            </Link>
          </li>
        );
      })}
    </ul>
  );
}

// --- the two lists ----------------------------------------------------------------------------

function BlockCard({
  title,
  meta,
  children,
  titleId,
}: {
  title: string;
  meta?: ReactNode;
  children: ReactNode;
  titleId: string;
}) {
  return (
    <Card aria-labelledby={titleId} className="content-start gap-3 py-4">
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 px-4">
        <h2 id={titleId} className="text-base font-semibold">
          {title}
        </h2>
        {meta ? <span className="text-xs text-muted-foreground">{meta}</span> : null}
      </div>
      <div className="px-4">{children}</div>
    </Card>
  );
}

/** The share of the largest figure each row's bar fills; an SVG attribute, since a style attribute is not allowed. */
function Meter({ share }: { share: number }) {
  return (
    <svg aria-hidden="true" className="mt-1 block h-1.5 w-full">
      <rect width="100%" height="100%" rx="3" className="fill-muted" />
      <rect width={`${Math.max(share, 2)}%`} height="100%" rx="3" className="fill-primary" />
    </svg>
  );
}

export function TopUsageCard({ query }: { query: UseQueryResult<TopUsage> }) {
  const { t, formatNumber, formatDateTime } = useI18n();
  const id = useId();
  const title = t("overview.usage.title");
  let body: ReactNode;
  if (query.isPending) body = <BlockSkeleton label={t("overview.loading")} />;
  else if (query.isError) body = <BlockError title={t("overview.usage.failed")} query={query} />;
  else if (query.data.devices.length === 0)
    body = <p className="text-sm text-muted-foreground">{t("overview.usage.empty")}</p>;
  else {
    const peak = Math.max(...query.data.devices.map((device) => device.kwh));
    body = (
      <ol className="grid gap-3">
        {query.data.devices.map((device) => (
          <li key={device.device_id} className="grid grid-cols-[minmax(0,1fr)_auto] items-baseline gap-x-3 text-sm">
            <span className="min-w-0">
              <Link
                to={devicePath(device.device_id)}
                className={cn("rounded-sm font-medium wrap-anywhere hover:underline", focusRing)}
              >
                {device.name}
              </Link>
              <Meter share={peak > 0 ? (device.kwh / peak) * 100 : 0} />
            </span>
            <span className="tabular-nums">{formatNumber(device.kwh)}</span>
          </li>
        ))}
      </ol>
    );
  }
  return (
    <BlockCard
      titleId={id}
      title={title}
      meta={query.data ? t("overview.usage.since", { time: formatDateTime(query.data.since) }) : undefined}
    >
      {body}
    </BlockCard>
  );
}

export function NewAnomaliesCard({ query }: { query: UseQueryResult<NewAnomalies> }) {
  const { t, formatDateTime } = useI18n();
  const id = useId();
  let body: ReactNode;
  if (query.isPending) body = <BlockSkeleton label={t("overview.loading")} />;
  else if (query.isError) body = <BlockError title={t("overview.anomalies.failed")} query={query} />;
  else if (query.data.anomalies.length === 0) {
    body = <p className="text-sm text-muted-foreground">{t("overview.anomalies.empty")}</p>;
  } else {
    body = (
      <ul className="grid gap-3">
        {query.data.anomalies.map((anomaly) => (
          <li
            key={`${anomaly.device_id}-${anomaly.started_at}`}
            className="grid grid-cols-[minmax(0,1fr)_auto] items-baseline gap-x-3 gap-y-0.5 text-sm"
          >
            <Link
              to={devicePath(anomaly.device_id)}
              className={cn("rounded-sm font-medium wrap-anywhere hover:underline", focusRing)}
            >
              {anomaly.name}
              <span className="font-normal"> · {anomaly.summary}</span>
            </Link>
            <StatusBadge status={anomaly.kind} />
            <span className="col-span-2 text-xs text-muted-foreground">
              {anomaly.ended_at
                ? t("overview.anomalies.period", {
                    start: formatDateTime(anomaly.started_at),
                    end: formatDateTime(anomaly.ended_at),
                  })
                : t("overview.anomalies.ongoing", { start: formatDateTime(anomaly.started_at) })}
            </span>
          </li>
        ))}
      </ul>
    );
  }
  return (
    <BlockCard
      titleId={id}
      title={t("overview.anomalies.title")}
      meta={query.data ? t("overview.anomalies.since", { time: formatDateTime(query.data.since) }) : undefined}
    >
      {body}
    </BlockCard>
  );
}

// --- the device table -------------------------------------------------------------------------

/** How many devices the table lists before 「show all」. */
const PREVIEW_ROWS = 8;

export function DeviceTable({
  query,
  canAddDevices,
  addDeviceLink,
}: {
  query: UseQueryResult<DeviceHealthRow[]>;
  canAddDevices: boolean;
  addDeviceLink: string | null;
}) {
  const { t, formatNumber, formatDateTime } = useI18n();
  const id = useId();
  const searchId = useId();
  const wide = useMediaQuery("(min-width: 768px)");
  const [search, setSearch] = useState("");
  const [all, setAll] = useState(false);
  const rows = query.data ?? [];
  const needle = search.trim().toLowerCase();
  const matching = rows.filter((row) => needle === "" || row.name.toLowerCase().includes(needle));
  const shown = all || needle !== "" ? matching : matching.slice(0, PREVIEW_ROWS);

  let body: ReactNode;
  if (query.isPending) body = <BlockSkeleton label={t("overview.loading")} />;
  else if (query.isError) body = <BlockError title={t("overview.devices.failed")} query={query} />;
  else if (rows.length === 0) {
    body = (
      <EmptyState
        icon={<UnplugIcon />}
        title={t("overview.devices.empty.title")}
        action={
          canAddDevices && addDeviceLink ? (
            <Button asChild size="sm">
              <Link to={addDeviceLink}>{t("setup.step.add_device.title")}</Link>
            </Button>
          ) : null
        }
      >
        <p>{canAddDevices ? t("overview.devices.empty.body") : t("overview.devices.empty.body_member")}</p>
      </EmptyState>
    );
  } else if (matching.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-2 px-4 py-8 text-center">
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <p className="font-semibold">{t("overview.devices.no_match.title")}</p>
        <p className="text-sm text-muted-foreground">{t("overview.devices.no_match.body")}</p>
      </div>
    );
  } else if (wide) {
    body = (
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b text-left text-xs text-muted-foreground">
            <th scope="col" className="py-2 pr-3 font-medium">
              {t("overview.devices.name")}
            </th>
            <th scope="col" className="py-2 pr-3 font-medium">
              {t("overview.devices.model")}
            </th>
            <th scope="col" className="py-2 pr-3 font-medium">
              {t("overview.devices.gateway")}
            </th>
            <th scope="col" className="py-2 pr-3 font-medium">
              {t("overview.devices.status")}
            </th>
            <th scope="col" className="py-2 pr-3 text-right font-medium">
              {t("overview.devices.usage")}
            </th>
            <th scope="col" className="py-2 text-right font-medium">
              {t("overview.devices.last_report")}
            </th>
          </tr>
        </thead>
        <tbody>
          {shown.map((row) => (
            <tr key={row.device_id} className="border-b last:border-b-0">
              <td className="py-2 pr-3">
                <Link
                  to={devicePath(row.device_id)}
                  className={cn("rounded-sm font-medium text-primary hover:underline", focusRing)}
                >
                  {row.name}
                </Link>
              </td>
              <td className="py-2 pr-3 text-muted-foreground">{row.model_name}</td>
              <td className="py-2 pr-3 text-muted-foreground">{row.gateway}</td>
              <td className="py-2 pr-3">
                <StatusBadge status={row.status} />
              </td>
              <td className="py-2 pr-3 text-right tabular-nums">
                {row.usage_today_kwh === null ? "—" : formatNumber(row.usage_today_kwh)}
              </td>
              <td className="py-2 text-right tabular-nums text-muted-foreground">
                {row.last_report_at ? formatDateTime(row.last_report_at) : "—"}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    );
  } else {
    body = (
      <ul className="grid">
        {shown.map((row) => (
          <li
            key={row.device_id}
            className="grid grid-cols-[minmax(0,1fr)_auto] items-baseline gap-x-3 gap-y-0.5 border-b py-2 text-sm last:border-b-0"
          >
            <Link
              to={devicePath(row.device_id)}
              className={cn("rounded-sm font-medium wrap-anywhere text-primary hover:underline", focusRing)}
            >
              {row.name}
            </Link>
            <StatusBadge status={row.status} />
            <span className="col-span-2 text-xs text-muted-foreground">
              {t("overview.devices.mobile_meta", {
                model: row.model_name,
                usage: row.usage_today_kwh === null ? "—" : formatNumber(row.usage_today_kwh),
                time: row.last_report_at ? formatDateTime(row.last_report_at) : "—",
              })}
            </span>
          </li>
        ))}
      </ul>
    );
  }

  const hidden = needle === "" && !all ? Math.max(matching.length - PREVIEW_ROWS, 0) : 0;
  return (
    <Card aria-labelledby={id} className="content-start gap-3 py-4">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4">
        <h2 id={id} className="text-base font-semibold">
          {t("overview.devices.title")}
        </h2>
        {query.data ? (
          <span className="text-xs text-muted-foreground">
            {t("overview.devices.count", { count: formatNumber(rows.length) })}
          </span>
        ) : null}
        {rows.length > 0 ? (
          <search className="relative ml-auto w-full sm:w-64">
            <label htmlFor={searchId} className="sr-only">
              {t("overview.devices.search")}
            </label>
            <SearchIcon
              aria-hidden="true"
              className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
            />
            <input
              id={searchId}
              type="search"
              value={search}
              placeholder={t("overview.devices.search")}
              className={cn(controlClasses, "h-8 pl-9")}
              onChange={(event) => setSearch(event.target.value)}
            />
          </search>
        ) : null}
      </div>
      <div className="px-4">{body}</div>
      {hidden > 0 ? (
        <div className="px-4">
          <Button variant="ghost" size="sm" onClick={() => setAll(true)}>
            {t("overview.devices.show_all", { count: formatNumber(rows.length) })}
          </Button>
        </div>
      ) : null}
    </Card>
  );
}
