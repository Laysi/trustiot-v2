import { RefreshCwIcon, TriangleAlertIcon } from "lucide-react";

import type { Device, DeviceModel } from "../../api/devices.ts";
import { DEVICES_PATH } from "../../app/paths.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { cn } from "../../lib/utils.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { DeviceAsk } from "./DeviceAsk.tsx";
import { type DeviceFormat, useDeviceFormat } from "./format.ts";
import type { DataHealth } from "./health.ts";

/**
 * The head of the device page (docs/design/device-page/): the name with the data's state, a line
 * of model, gateway and polling interval, and the refresh. The state answers the first question
 * of the page, whether data is coming in; the 「ask AI」 button of the board joins the actions with
 * the home composer (Laysi/trustiot-v2#928).
 */
export function DeviceHeader({
  device,
  model,
  health,
  now,
  refreshing,
  updatedAt,
  onRefresh,
}: {
  device: Device;
  model: DeviceModel | undefined;
  health: DataHealth;
  now: number;
  refreshing: boolean;
  updatedAt: number;
  onRefresh(): void;
}) {
  const format = useDeviceFormat();
  const { t } = format;
  return (
    <PageHeader
      title={device.name}
      crumbs={[{ label: t("devices.title"), to: DEVICES_PATH }]}
      tags={<StatusChip health={health} now={now} format={format} />}
      meta={
        <p className="flex flex-wrap gap-x-3.5 gap-y-1">
          <span>
            {t("device.meta.model")} <b className="font-mono font-medium text-foreground">{model?.name ?? "…"}</b>
          </span>
          <span>
            {t("device.meta.gateway")} <b className="font-mono font-medium text-foreground">{device.hardware_id}</b>
          </span>
          <span>{t("device.meta.poll", { duration: format.duration(device.poll_interval_ms) })}</span>
        </p>
      }
      actions={
        <>
          <DeviceAsk device={device} health={health} now={now} />
          <p role="status" className="text-[13px] text-muted-foreground">
            {refreshing
              ? t("device.refreshing")
              : updatedAt > 0
                ? t("device.updated_at", { time: format.time(updatedAt) })
                : null}
          </p>
          <Button variant="outline" onClick={onRefresh} disabled={refreshing}>
            <RefreshCwIcon className={refreshing ? "animate-spin" : undefined} aria-hidden="true" />
            {t("device.refresh")}
          </Button>
        </>
      }
    />
  );
}

/** The data's state as a dot or an icon and words, never color alone. */
function StatusChip({ health, now, format }: { health: DataHealth; now: number; format: DeviceFormat }) {
  const { t } = format;
  const chip =
    "inline-flex h-6.5 items-center gap-1.5 rounded-full border bg-background px-2.5 text-[13px] font-medium whitespace-nowrap";
  switch (health.kind) {
    case "unknown":
      return <span aria-hidden="true" className="inline-block h-6.5 w-36 animate-pulse rounded-full bg-muted" />;
    case "none":
      return (
        <span role="status" className={chip}>
          <span aria-hidden="true" className="size-2 rounded-full bg-muted-foreground" />
          {t("device.status.none")}
        </span>
      );
    case "ok":
      return (
        <span role="status" className={chip}>
          <span aria-hidden="true" className="size-2 rounded-full bg-success" />
          {t("device.status.ok", { ago: format.ago(now - health.newest) })}
        </span>
      );
    case "stale":
      return (
        <span role="status" className={cn(chip, "border-warning/40 bg-warning-soft")}>
          <TriangleAlertIcon className="size-4 text-warning" aria-hidden="true" />
          {t("device.status.stale", { ago: format.ago(now - health.newest) })}
        </span>
      );
  }
}

/** Under the head while the data is stalled: for how long, and what to check first. */
export function StaleBanner({ device, newest, now }: { device: Device; newest: number; now: number }) {
  const format = useDeviceFormat();
  const { t } = format;
  return (
    <Alert role="status" className="grid-cols-[1rem_1fr] border-warning/35 bg-warning-soft">
      <TriangleAlertIcon className="text-warning" aria-hidden="true" />
      <AlertDescription className="gap-1.5 text-foreground">
        <p className="font-semibold">{t("device.stale.title", { duration: format.elapsed(now - newest) })}</p>
        <p>
          {t("device.stale.body", {
            time: format.time(newest),
            poll: format.duration(device.poll_interval_ms),
            gateway: device.hardware_id,
            address: `${device.host}:${device.port}`,
          })}
        </p>
      </AlertDescription>
    </Alert>
  );
}
