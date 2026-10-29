import type { UseQueryResult } from "@tanstack/react-query";
import { ChartLineIcon, InboxIcon, ListPlusIcon } from "lucide-react";

import type { Device, DeviceModel, Latest } from "../../api/devices.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "../../components/ui/card.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { EmptyState } from "./EmptyState.tsx";
import { useDeviceFormat } from "./format.ts";
import { chartablePoints } from "./points.ts";

/**
 * The newest stored reading of each data point (`GET /api/telemetry/v1/devices/{id}/latest`),
 * listed in the model's order. Each row opens that point's chart below.
 */
export function LatestCard({
  device,
  model,
  latest,
  selected,
  onShowChart,
}: {
  device: Device;
  model: UseQueryResult<DeviceModel>;
  latest: UseQueryResult<Latest>;
  selected: string | null;
  onShowChart(key: string): void;
}) {
  const { errorMessage } = useI18n();
  const format = useDeviceFormat();
  const { t } = format;
  const points = chartablePoints(model.data, latest.data);
  const readings = new Map((latest.data?.points ?? []).map((reading) => [reading.point, reading]));

  return (
    <Card aria-labelledby="device-latest">
      <CardHeader>
        <CardTitle id="device-latest">{t("device.latest.title")}</CardTitle>
        <CardDescription>
          {latest.data
            ? t("device.latest.description_since", { earliest: format.minute(latest.data.earliest) })
            : t("device.latest.description")}
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4">
        {model.isError ? (
          <Alert role="status">
            <AlertDescription>
              <p>{t("device.model_failed", { reason: errorMessage(model.error) })}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        {latest.isError ? (
          <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
            <AlertDescription className="col-start-1">
              <p>{t("device.latest.failed", { reason: errorMessage(latest.error) })}</p>
            </AlertDescription>
            <Button variant="outline" onClick={() => void latest.refetch()} disabled={latest.isFetching}>
              {t("page.action.retry")}
            </Button>
          </Alert>
        ) : null}
        {model.isPending ? (
          <Spinner label={t("device.latest.loading")} />
        ) : points.length === 0 && !latest.isPending ? (
          <EmptyState icon={<ListPlusIcon />} title={t("device.latest.no_points_title")}>
            <p>{t("device.latest.no_points_body", { model: model.data?.name ?? "" })}</p>
          </EmptyState>
        ) : (
          <>
            {latest.data && latest.data.points.length === 0 ? (
              <EmptyState icon={<InboxIcon />} title={t("device.latest.empty_title")}>
                <p>
                  {t("device.latest.empty_body", {
                    earliest: format.minute(latest.data.earliest),
                    gateway: device.hardware_id,
                  })}
                </p>
              </EmptyState>
            ) : null}
            <div className="overflow-x-auto">
              <table className="w-full text-sm" aria-busy={latest.isPending}>
                <thead>
                  <tr className="border-b text-left text-muted-foreground">
                    <th scope="col" className="py-2 pr-4 font-medium">
                      {t("device.latest.point")}
                    </th>
                    <th scope="col" className="py-2 pr-4 font-medium">
                      {t("device.latest.value")}
                    </th>
                    <th scope="col" className="py-2 pr-4 font-medium">
                      {t("device.latest.time")}
                    </th>
                    <th scope="col" className="py-2 text-right font-medium">
                      <span className="sr-only">{t("device.latest.chart")}</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {points.map((point) => {
                    const reading = readings.get(point.key);
                    const isSelected = point.key === selected;
                    return (
                      <tr key={point.key} className={cn("border-b last:border-b-0", isSelected && "bg-accent/60")}>
                        <th scope="row" className="py-2.5 pr-4 text-left align-top font-normal">
                          <span className="font-mono font-medium wrap-break-word">{point.key}</span>
                          {point.description ? (
                            <span className="block text-muted-foreground">{point.description}</span>
                          ) : null}
                          {!point.inModel ? (
                            <span className="block text-xs text-muted-foreground">
                              {t("device.latest.not_in_model")}
                            </span>
                          ) : null}
                        </th>
                        <td className="py-2.5 pr-4 align-top tabular-nums">
                          {latest.isPending ? (
                            <span className="text-muted-foreground">…</span>
                          ) : reading ? (
                            <span className="font-medium wrap-break-word">{format.reading(reading.value)}</span>
                          ) : (
                            <span className="text-muted-foreground">{t("device.latest.no_reading")}</span>
                          )}
                        </td>
                        <td className="py-2.5 pr-4 align-top text-muted-foreground tabular-nums">
                          {reading ? <time dateTime={reading.time}>{format.time(reading.time)}</time> : null}
                        </td>
                        <td className="py-1.5 text-right align-top">
                          <Button
                            variant="ghost"
                            size="icon"
                            aria-label={t("device.latest.show_chart", { point: point.key })}
                            aria-pressed={isSelected}
                            onClick={() => onShowChart(point.key)}
                          >
                            <ChartLineIcon />
                          </Button>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}
