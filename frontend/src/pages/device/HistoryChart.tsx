import { useCallback, useMemo } from "react";

import { Button } from "../../components/ui/button.tsx";
import type { ChartEnvironment, ZoomWindow } from "../../dashboard/chart/InteractiveChart.tsx";
import { InteractiveChart } from "../../dashboard/chart/InteractiveChart.tsx";
import { DashboardEnvProvider } from "../../dashboard/context.tsx";
import { dashboardFormat } from "../../dashboard/format.ts";
import { TipRow } from "../../dashboard/parts.tsx";
import type { Datum, Gap } from "./chart.ts";
import type { DeviceFormat } from "./format.ts";
import { fromWall, historyModel, historyOption } from "./historyOption.ts";

const MINUTE = 60_000;

export interface HistoryChartProps {
  data: readonly Datum[];
  kind: "number" | "bool";
  gaps: readonly Gap[];
  /** The range the chart spans, as instants. */
  from: number;
  to: number;
  /** Names the chart for assistive technology. */
  label: string;
  /** What the data point is called (the readings list's label). */
  title: string;
  format: DeviceFormat;
  /** The zoomed window as instants, or `null` once the chart is back to the full range. */
  onZoom(window: { from: number; to: number } | null): void;
  /** The zoomed window made the range of the page. */
  onSetRange(window: { from: number; to: number }): void;
  /** Enter or a click on the chart: the buckets as a table (the chart has no other source to open). */
  onOpenTable(): void;
}

/**
 * One data point's history on the dashboard's time-series chart (dashboard/chart/): the same
 * ECharts options, tooltip, crosshair, zoom and keyboard slider an answer's chart has. Each
 * stop of the slider is a bucket of the series answer; the tooltip names its time, its mean,
 * minimum and maximum, and the readings behind it. The figures above the chart are the page's
 * (HistoryCard), and they follow the zoom through `onZoom`.
 */
export function HistoryChart(props: HistoryChartProps) {
  const { data, kind, gaps, from, to, format, title } = props;
  const { t, locale, timeZone } = format;
  const env = useMemo(() => dashboardFormat(locale, timeZone), [locale, timeZone]);
  const model = useMemo(() => historyModel(data, gaps, timeZone), [data, gaps, timeZone]);

  const buildOption = useCallback(
    (environment: ChartEnvironment) =>
      historyOption(
        {
          data,
          kind,
          model,
          gaps,
          from,
          to,
          timeZone,
          gapLabel: (gap) => t("device.history.gap_label", { duration: format.elapsed(gap.to - gap.from) }),
          percent: format.share,
        },
        env,
        environment,
        title,
        props.label,
      ),
    [data, kind, model, gaps, from, to, timeZone, t, format, env, title, props.label],
  );

  const slots = useMemo(() => [0], []);
  const none = useMemo(() => new Set<number>(), []);
  const datumAt = (stop: number) => data[stop];

  const describe = (stop: number) => {
    const datum = datumAt(stop);
    if (datum === undefined) return "";
    return kind === "bool"
      ? t("device.history.readout_bool", {
          time: format.minute(datum.time),
          share: format.share(datum.value),
          count: datum.count,
        })
      : t("device.history.readout", {
          time: format.minute(datum.time),
          mean: format.number(datum.value),
          min: format.number(datum.min ?? datum.value),
          max: format.number(datum.max ?? datum.value),
          count: datum.count,
        });
  };

  return (
    <DashboardEnvProvider locale={locale} timeZone={timeZone}>
      <InteractiveChart
        label={props.label}
        size="chart"
        buildOption={buildOption}
        slots={slots}
        hiddenSlots={none}
        zoom
        stopCount={data.length}
        hitOf={(stop) => {
          const index = model.pointOfStop[stop];
          return index === undefined || index < 0 ? null : { seriesIndex: 0, dataIndex: index };
        }}
        stopOf={(hit) => (hit.seriesIndex === 0 ? (model.stopOfPoint.get(hit.dataIndex) ?? null) : null)}
        describe={describe}
        renderTip={(stop) => {
          const datum = datumAt(stop);
          if (datum === undefined) return null;
          return (
            <div className="grid gap-1">
              <p className="font-medium tabular-nums" data-testid="tip-time">
                {format.minute(datum.time)}
              </p>
              {kind === "bool" ? (
                <TipRow label={t("device.history.on_share")} value={format.share(datum.value)} />
              ) : (
                <>
                  <TipRow label={t("device.history.mean")} value={format.number(datum.value)} />
                  <TipRow label={t("device.history.min")} value={format.number(datum.min ?? datum.value)} />
                  <TipRow label={t("device.history.max")} value={format.number(datum.max ?? datum.value)} />
                </>
              )}
              <TipRow label={t("device.history.readings")} value={format.number(datum.count)} />
            </div>
          );
        }}
        onSource={() => props.onOpenTable()}
        onZoom={(window) =>
          props.onZoom(
            window === null ? null : { from: fromWall(window.from, timeZone), to: fromWall(window.to, timeZone) },
          )
        }
        zoomActions={(window: ZoomWindow) => {
          const visible = { from: fromWall(window.from, timeZone), to: fromWall(window.to, timeZone) };
          return (
            <>
              <p className="text-xs text-muted-foreground" role="status">
                {t("device.history.zoomed", { from: format.minute(visible.from), to: format.minute(visible.to) })}
              </p>
              <Button
                size="sm"
                onClick={() =>
                  props.onSetRange({
                    from: Math.floor(visible.from / MINUTE) * MINUTE,
                    to: Math.ceil(visible.to / MINUTE) * MINUTE,
                  })
                }
              >
                {t("device.history.zoom_set_range")}
              </Button>
            </>
          );
        }}
      />
    </DashboardEnvProvider>
  );
}
