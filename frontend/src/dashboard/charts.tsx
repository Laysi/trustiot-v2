// The two chart components, time series and bar, on top of InteractiveChart: what a stop is, what
// the tooltip and the slider's value text say about it, and which rows are its source.
import { useCallback, useMemo, useState } from "react";

import type { ChartEnvironment, ChartHit } from "./chart/InteractiveChart.tsx";
import { InteractiveChart } from "./chart/InteractiveChart.tsx";
import { barOption, sparkOption, timeSeriesOption } from "./chart/options.ts";
import { useDashboardEnv } from "./context.tsx";
import type { BarView, Point, SeriesView, SourceRef, TimeSeriesView } from "./model.ts";
import { Legend, TipRow } from "./parts.tsx";

/** The series the legend hides, by key, so a series stays hidden when new data moves its slot. */
function useHidden(): [ReadonlySet<string>, (key: string) => void] {
  const [hidden, setHidden] = useState<ReadonlySet<string>>(() => new Set());
  const toggle = useCallback((key: string) => {
    setHidden((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }, []);
  return [hidden, toggle];
}

/** What InteractiveChart needs of the legend: the series' slots and the hidden ones. */
function useSlots(
  series: readonly { key: string; slot: number }[],
  hidden: ReadonlySet<string>,
): [readonly number[], ReadonlySet<number>] {
  const slots = useMemo(() => series.map((item) => item.slot), [series]);
  const hiddenSlots = useMemo(
    () => new Set(series.filter((item) => hidden.has(item.key)).map((item) => item.slot)),
    [series, hidden],
  );
  return [slots, hiddenSlots];
}

interface LinePoint {
  series: SeriesView;
  point: Point;
  dataIndex: number;
}

/** For each series, where each x is in its points. */
function pointIndex(series: readonly SeriesView[]): ReadonlyArray<ReadonlyMap<number, number>> {
  return series.map((line) => new Map(line.points.map((point, index) => [point.x, index])));
}

export function TimeSeriesBody(props: { view: TimeSeriesView; onSource(source: SourceRef): void }) {
  const { view, onSource } = props;
  const { t, format } = useDashboardEnv();
  const [hidden, toggle] = useHidden();
  const label = t("dashboard.chart.label", { title: view.title });
  const indexes = useMemo(() => pointIndex(view.series), [view]);
  const stopOfX = useMemo(() => new Map(view.xs.map((x, stop) => [x, stop])), [view]);
  const [slots, hiddenSlots] = useSlots(view.series, hidden);
  const buildOption = useCallback(
    (environment: ChartEnvironment) => timeSeriesOption(view, format, environment, label),
    [view, format, label],
  );

  const pointsAt = (stop: number): LinePoint[] => {
    const x = view.xs[stop];
    if (x === undefined) return [];
    return view.series.flatMap((series, slot) => {
      if (hidden.has(series.key)) return [];
      const dataIndex = indexes[slot]?.get(x);
      const point = dataIndex === undefined ? undefined : series.points[dataIndex];
      return point === undefined || dataIndex === undefined ? [] : [{ series, point, dataIndex }];
    });
  };
  const value = (point: Point) => format.withUnit(format.number(point.y), view.unit);
  const time = (stop: number) => format.wallTime(view.xs[stop] ?? 0, view.dateOnly);

  return (
    <div className="grid gap-3">
      <Legend entries={view.series} hidden={hidden} onToggle={toggle} />
      <InteractiveChart
        label={label}
        size="chart"
        buildOption={buildOption}
        slots={slots}
        hiddenSlots={hiddenSlots}
        zoom
        stopCount={view.xs.length}
        hitOf={(stop) => {
          const first = pointsAt(stop)[0];
          return first === undefined ? null : { seriesIndex: first.series.slot, dataIndex: first.dataIndex };
        }}
        stopOf={(hit: ChartHit) => {
          const point = view.series[hit.seriesIndex]?.points[hit.dataIndex];
          return point === undefined ? null : (stopOfX.get(point.x) ?? null);
        }}
        describe={(stop) =>
          [time(stop), ...pointsAt(stop).map(({ series, point }) => `${series.label} ${value(point)}`)].join(", ")
        }
        renderTip={(stop) => (
          <div className="grid gap-1">
            <p className="font-medium tabular-nums" data-testid="tip-time">
              {time(stop)}
            </p>
            {pointsAt(stop).map(({ series, point }) => (
              <TipRow key={series.key} slot={series.slot} label={series.label} value={value(point)} />
            ))}
          </div>
        )}
        onSource={(stop) =>
          onSource({ kind: "rows", dataset: view.dataset, rows: pointsAt(stop).map(({ point }) => point.row) })
        }
      />
    </div>
  );
}

export function BarBody(props: { view: BarView; onSource(source: SourceRef): void }) {
  const { view, onSource } = props;
  const { t, format } = useDashboardEnv();
  const [hidden, toggle] = useHidden();
  const label = t("dashboard.chart.label", { title: view.title });
  const [slots, hiddenSlots] = useSlots(view.series, hidden);
  const buildOption = useCallback(
    (environment: ChartEnvironment) => barOption(view, format, environment, label),
    [view, format, label],
  );
  const category = (stop: number) => {
    const item = view.categories[stop];
    if (item === undefined) return "";
    return item.time === null ? item.label : format.wallTime(item.time, view.dateOnly);
  };
  const barsAt = (stop: number) =>
    view.series.flatMap((series) => {
      const bar = series.values[stop];
      return hidden.has(series.key) || bar == null ? [] : [{ series, bar }];
    });
  const value = (y: number | null) => format.withUnit(format.number(y), view.unit);

  return (
    <div className="grid gap-3">
      <Legend entries={view.series} hidden={hidden} onToggle={toggle} />
      <InteractiveChart
        label={label}
        size="chart"
        buildOption={buildOption}
        slots={slots}
        hiddenSlots={hiddenSlots}
        zoom={false}
        stopCount={view.categories.length}
        hitOf={(stop) => {
          const first = barsAt(stop)[0];
          return first === undefined ? null : { seriesIndex: first.series.slot, dataIndex: stop };
        }}
        stopOf={(hit) => (hit.dataIndex >= 0 && hit.dataIndex < view.categories.length ? hit.dataIndex : null)}
        describe={(stop) =>
          [category(stop), ...barsAt(stop).map(({ series, bar }) => `${series.label} ${value(bar.y)}`)].join(", ")
        }
        renderTip={(stop) => (
          <div className="grid gap-1">
            <p className="font-medium" data-testid="tip-time">
              {category(stop)}
            </p>
            {barsAt(stop).map(({ series, bar }) => (
              <TipRow key={series.key} slot={series.slot} label={series.label} value={value(bar.y)} />
            ))}
          </div>
        )}
        onSource={(stop) =>
          onSource({ kind: "rows", dataset: view.dataset, rows: barsAt(stop).map(({ bar }) => bar.row) })
        }
      />
    </div>
  );
}

/** The sparkline's one series, in slot 0, never hidden. */
const SPARK_SLOTS: readonly number[] = [0];
const NO_HIDDEN: ReadonlySet<number> = new Set();

/** The small trend under a KPI: one line, its tooltip and source, no axes and no zoom. */
export function Sparkline(props: {
  series: SeriesView;
  dataset: string;
  dateOnly: boolean;
  unit: string | undefined;
  title: string;
  onSource(source: SourceRef): void;
}) {
  const { series, onSource } = props;
  const { t, format } = useDashboardEnv();
  const label = t("dashboard.chart.trend_label", { title: props.title });
  const buildOption = useCallback(
    (environment: ChartEnvironment) => sparkOption(series, environment, label),
    [series, label],
  );
  const text = (stop: number) => {
    const point = series.points[stop];
    if (point === undefined) return "";
    return `${format.wallTime(point.x, props.dateOnly)}, ${format.withUnit(format.number(point.y), props.unit)}`;
  };
  return (
    <InteractiveChart
      label={label}
      size="spark"
      buildOption={buildOption}
      slots={SPARK_SLOTS}
      hiddenSlots={NO_HIDDEN}
      zoom={false}
      stopCount={series.points.length}
      hitOf={(stop) => (series.points[stop] ? { seriesIndex: 0, dataIndex: stop } : null)}
      stopOf={(hit) => (hit.seriesIndex === 0 && series.points[hit.dataIndex] ? hit.dataIndex : null)}
      describe={text}
      renderTip={(stop) => {
        const point = series.points[stop];
        return point === undefined ? null : (
          <div className="grid gap-1">
            <p className="font-medium tabular-nums" data-testid="tip-time">
              {format.wallTime(point.x, props.dateOnly)}
            </p>
            <TipRow label={series.label} value={format.withUnit(format.number(point.y), props.unit)} />
          </div>
        );
      }}
      onSource={(stop) => {
        const point = series.points[stop];
        if (point) onSource({ kind: "rows", dataset: props.dataset, rows: [point.row] });
      }}
    />
  );
}
