import type { UseQueryResult } from "@tanstack/react-query";
import { ChevronDownIcon, ChevronUpIcon, ClockIcon, TriangleAlertIcon } from "lucide-react";
import { useState } from "react";

import type { DeviceModel, Latest, Reading } from "../../api/devices.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { Card, CardHeader, CardTitle } from "../../components/ui/card.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { type DeviceFormat, useDeviceFormat } from "./format.ts";
import { staleAfterMs } from "./health.ts";
import { chartablePoints, type PointInfo } from "./points.ts";

/** How many data points the list shows before 「show the others」. */
export const VISIBLE_POINTS = 8;

/**
 * The newest stored reading of each data point (`GET /api/telemetry/v1/devices/{id}/latest`),
 * listed in the model's order, as docs/design/device-page/ draws it: the point's description (or
 * its key), the value, and how long ago. A row is a button that puts that point on the chart.
 */
export function LatestCard({
  model,
  latest,
  selected,
  onSelect,
  pollIntervalMs,
  now,
}: {
  model: UseQueryResult<DeviceModel>;
  latest: UseQueryResult<Latest>;
  selected: string | null;
  onSelect(key: string): void;
  pollIntervalMs: number;
  now: number;
}) {
  const { errorMessage } = useI18n();
  const format = useDeviceFormat();
  const { t } = format;
  const [expanded, setExpanded] = useState(false);
  const points = chartablePoints(model.data, latest.data);
  const readings = new Map((latest.data?.points ?? []).map((reading) => [reading.point, reading]));
  const selectedIndex = points.findIndex((point) => point.key === selected);
  // A point chosen by a link that is past the first few stays visible.
  const showAll = expanded || selectedIndex >= VISIBLE_POINTS;
  const shown = showAll ? points : points.slice(0, VISIBLE_POINTS);
  const loading = model.isPending || latest.isPending;

  return (
    <Card aria-labelledby="device-latest" className="gap-3 py-0">
      <CardHeader className="pt-5">
        <CardTitle id="device-latest" className="text-base">
          {t("device.latest.title")}
        </CardTitle>
        <p className="col-start-2 row-start-1 pt-0.5 text-xs text-muted-foreground">{t("device.latest.hint")}</p>
      </CardHeader>
      {model.isError ? (
        <div className="px-4">
          <Alert role="status" className="grid-cols-[1rem_1fr_auto] items-center border-warning/40 bg-warning-soft">
            <TriangleAlertIcon className="text-warning" aria-hidden="true" />
            <AlertDescription className="text-foreground">
              <p>{t("device.model_failed", { reason: errorMessage(model.error) })}</p>
            </AlertDescription>
            <Button variant="ghost" size="sm" onClick={() => void model.refetch()} disabled={model.isFetching}>
              {t("page.action.retry")}
            </Button>
          </Alert>
        </div>
      ) : null}
      {latest.isError ? (
        <div className="px-4">
          <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
            <AlertDescription className="col-start-1">
              <p>{t("device.latest.failed", { reason: errorMessage(latest.error) })}</p>
            </AlertDescription>
            <Button variant="outline" size="sm" onClick={() => void latest.refetch()} disabled={latest.isFetching}>
              {t("page.action.retry")}
            </Button>
          </Alert>
        </div>
      ) : null}
      {loading && !latest.isError ? (
        <LatestSkeleton label={t("device.latest.loading")} />
      ) : (
        <ul className="m-0 grid list-none gap-0.5 p-2 pt-0">
          {shown.map((point) => (
            <li key={point.key}>
              <ReadingRow
                point={point}
                reading={readings.get(point.key)}
                selected={point.key === selected}
                stale={
                  readings.has(point.key) &&
                  now - Date.parse(readings.get(point.key)?.time ?? "") > staleAfterMs(pollIntervalMs)
                }
                now={now}
                format={format}
                onSelect={() => onSelect(point.key)}
              />
            </li>
          ))}
        </ul>
      )}
      {!loading && points.length > VISIBLE_POINTS && selectedIndex < VISIBLE_POINTS ? (
        <div className="px-2 pb-2">
          <Button
            variant="ghost"
            size="sm"
            className="w-full"
            aria-expanded={expanded}
            onClick={() => setExpanded(!expanded)}
          >
            {expanded ? <ChevronUpIcon /> : <ChevronDownIcon />}
            {expanded
              ? t("device.latest.show_fewer")
              : t("device.latest.show_more", { count: points.length - VISIBLE_POINTS })}
          </Button>
        </div>
      ) : null}
    </Card>
  );
}

function ReadingRow({
  point,
  reading,
  selected,
  stale,
  now,
  format,
  onSelect,
}: {
  point: PointInfo;
  reading: Latest["points"][number] | undefined;
  selected: boolean;
  stale: boolean;
  now: number;
  format: DeviceFormat;
  onSelect(): void;
}) {
  const { t } = format;
  const label = point.description ?? point.key;
  return (
    <button
      type="button"
      aria-pressed={selected}
      onClick={onSelect}
      className={cn(
        "grid w-full grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 gap-y-0.5 rounded-lg border border-transparent px-3 py-2.5 text-left transition-colors hover:bg-accent",
        selected && "border-primary/55 bg-brand-soft hover:bg-brand-soft",
        focusRing,
      )}
    >
      <span className="grid min-w-0">
        <span className={cn("truncate text-sm font-medium", point.description === null && "font-mono")} title={label}>
          {label}
        </span>
        {point.description !== null ? (
          <span className="truncate font-mono text-xs text-muted-foreground">{point.key}</span>
        ) : null}
        {!point.inModel ? (
          <span className="text-xs text-muted-foreground">{t("device.latest.not_in_model")}</span>
        ) : null}
      </span>
      <span className="grid justify-items-end gap-0.5">
        {reading ? (
          <Value reading={reading.value} format={format} />
        ) : (
          <span className="text-sm text-muted-foreground">{t("device.latest.no_reading")}</span>
        )}
        {reading ? (
          <time
            dateTime={reading.time}
            title={format.time(reading.time)}
            className={cn("flex items-center gap-1 text-xs text-muted-foreground", stale && "font-medium text-warning")}
          >
            {stale ? <ClockIcon className="size-3" aria-hidden="true" /> : null}
            {format.ago(now - Date.parse(reading.time))}
          </time>
        ) : null}
      </span>
    </button>
  );
}

/** A value by its type: numbers large, a bool as a pill (shape and word, not only color), text in mono. */
function Value({ reading, format }: { reading: Reading; format: DeviceFormat }) {
  switch (reading.type) {
    case "bool":
      return (
        <span
          className={cn(
            "inline-flex h-6.5 items-center gap-1.5 rounded-full border px-2.5 text-sm font-semibold",
            reading.value && "border-primary/40 bg-brand-soft",
          )}
        >
          <span
            aria-hidden="true"
            className={cn(
              "size-2 rounded-full",
              reading.value ? "bg-primary" : "size-2.25 border-2 border-muted-foreground",
            )}
          />
          {format.reading(reading)}
        </span>
      );
    case "string":
      return (
        <span className="max-w-[12em] truncate font-mono text-sm font-medium" title={reading.value}>
          {reading.value}
        </span>
      );
    case "int":
      return <span className="text-xl leading-tight font-semibold tabular-nums">{format.number(reading.value)}</span>;
    case "float":
      return (
        <span className="text-xl leading-tight font-semibold tabular-nums">
          {reading.value === null ? format.reading(reading) : format.fixed(reading.value)}
        </span>
      );
  }
}

function LatestSkeleton({ label }: { label: string }) {
  return (
    <div role="status" aria-busy="true" className="grid gap-0.5 p-2 pt-0">
      <span className="sr-only">{label}</span>
      {[0, 1, 2, 3, 4].map((row) => (
        <div key={row} aria-hidden="true" className="flex items-center justify-between gap-3 px-3 py-2.5">
          <div className="grid gap-1.5">
            <div className="h-3.5 w-24 animate-pulse rounded bg-muted" />
            <div className="h-3 w-16 animate-pulse rounded bg-muted" />
          </div>
          <div className="grid justify-items-end gap-1.5">
            <div className="h-5 w-16 animate-pulse rounded bg-muted" />
            <div className="h-3 w-10 animate-pulse rounded bg-muted" />
          </div>
        </div>
      ))}
    </div>
  );
}
