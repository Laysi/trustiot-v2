import { HistoryIcon, InboxIcon, ListIcon, RefreshCwIcon } from "lucide-react";

import type { Device, DeviceModel, Latest } from "../../api/devices.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Card, CardContent, CardHeader, CardTitle } from "../../components/ui/card.tsx";
import { useDeviceFormat } from "./format.ts";

/** How the device is reached, for the facts and for the checklist of a device without readings. */
function connection(device: Device, t: ReturnType<typeof useDeviceFormat>["t"]): string {
  const transport = device.transport === "modbus_tcp" ? "Modbus TCP" : "Modbus RTU over TCP";
  return t("device.info.connection_value", {
    transport,
    address: `${device.host}:${device.port}`,
    unit: device.unit_id,
  });
}

/** What only matters when something is wrong, so it comes last: connection, unit and when it was added. */
export function DeviceInfo({ device, model }: { device: Device; model: DeviceModel | undefined }) {
  const format = useDeviceFormat();
  const { t } = format;
  const facts: { label: string; value: string; mono?: boolean }[] = [
    { label: t("device.info.model"), value: model?.name ?? "…", mono: true },
    { label: t("device.info.gateway"), value: device.hardware_id, mono: true },
    { label: t("device.info.connection"), value: connection(device, t) },
    {
      label: t("device.info.poll"),
      value: t("device.info.poll_every", { duration: format.duration(device.poll_interval_ms) }),
    },
    { label: t("device.info.created"), value: format.date(device.created_at) },
  ];
  return (
    <Card aria-labelledby="device-info" className="gap-3 py-0">
      <CardHeader className="pt-5">
        <CardTitle id="device-info" className="text-base">
          {t("device.info.title")}
        </CardTitle>
      </CardHeader>
      <CardContent className="pb-5">
        <dl className="m-0 grid gap-3 text-sm">
          {facts.map((fact) => (
            <div key={fact.label} className="grid min-w-0 gap-px">
              <dt className="text-[13px] text-muted-foreground">{fact.label}</dt>
              <dd className={fact.mono ? "m-0 font-mono wrap-anywhere" : "m-0 wrap-anywhere"}>{fact.value}</dd>
            </div>
          ))}
        </dl>
      </CardContent>
    </Card>
  );
}

/**
 * The device has stored no reading in the raw period: no chart, and the checks in the order they
 * most often find the cause (the board's `never` state).
 */
export function NoReadings({
  device,
  model,
  latest,
  checking,
  onCheck,
  onShowHistory,
}: {
  device: Device;
  model: DeviceModel | undefined;
  latest: Latest;
  checking: boolean;
  onCheck(): void;
  /** Older readings may exist beyond the raw period: open the history over the longest preset. */
  onShowHistory?: (() => void) | undefined;
}) {
  const format = useDeviceFormat();
  const { t } = format;
  const steps = [
    t("device.never.step_gateway", { gateway: device.hardware_id }),
    t("device.never.step_connection", { connection: connection(device, t) }),
    t("device.never.step_model", { model: model?.name ?? "…" }),
  ];
  return (
    <Card aria-labelledby="device-never" className="gap-0 py-0">
      <CardHeader className="pt-5">
        <CardTitle id="device-never" className="text-base">
          {t("device.latest.title")}
        </CardTitle>
      </CardHeader>
      <EmptyState
        centered
        icon={<InboxIcon aria-hidden="true" />}
        title={t("device.never.title")}
        action={
          <>
            <Button variant="outline" onClick={onCheck} disabled={checking}>
              <RefreshCwIcon className={checking ? "animate-spin" : undefined} aria-hidden="true" />
              {t("device.never.recheck")}
            </Button>
            {onShowHistory ? (
              <Button variant="outline" onClick={onShowHistory}>
                <HistoryIcon aria-hidden="true" />
                {t("device.never.older")}
              </Button>
            ) : null}
          </>
        }
      >
        <p>{t("device.never.body", { earliest: format.minute(latest.earliest) })}</p>
        <ol className="m-0 mt-2 grid list-none gap-2 p-0 text-left text-foreground">
          {steps.map((step, index) => (
            <li key={step} className="grid grid-cols-[24px_1fr] items-start gap-2">
              <span
                aria-hidden="true"
                className="flex size-5.5 items-center justify-center rounded-full border text-xs font-semibold text-muted-foreground"
              >
                {index + 1}
              </span>
              <span>{step}</span>
            </li>
          ))}
        </ol>
      </EmptyState>
    </Card>
  );
}

/**
 * The model has no data point, so the gateway reads nothing from this device. Text only: the
 * model page that could add them is Laysi/trustiot-v2#126 (decision of 2026-11-02), so there is
 * no link or button yet.
 */
export function NoDataPoints({ model }: { model: DeviceModel | undefined }) {
  const { t } = useDeviceFormat();
  return (
    <Card aria-labelledby="device-nopoints" className="gap-0 py-0">
      <CardHeader className="pt-5">
        <CardTitle id="device-nopoints" className="text-base">
          {t("device.latest.title")}
        </CardTitle>
      </CardHeader>
      <EmptyState
        centered
        icon={<ListIcon aria-hidden="true" />}
        title={t("device.latest.no_points_title", { model: model?.name ?? "" })}
      >
        <p>{t("device.latest.no_points_body")}</p>
      </EmptyState>
    </Card>
  );
}
