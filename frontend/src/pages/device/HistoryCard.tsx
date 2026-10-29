import { type UseQueryResult, useQuery } from "@tanstack/react-query";
import { CalendarClockIcon, CalendarXIcon, ChartLineIcon, InboxIcon, TableIcon, TypeIcon } from "lucide-react";
import { type FormEvent, useCallback, useId, useState } from "react";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { Series } from "../../api/devices.ts";
import { ApiError } from "../../api/errors.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "../../components/ui/card.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { chartData, type Datum, summary } from "./chart.ts";
import { EmptyState } from "./EmptyState.tsx";
import { type DeviceFormat, INTERVAL_LABELS, useDeviceFormat } from "./format.ts";
import type { PointInfo } from "./points.ts";
import { seriesKey } from "./queries.ts";
import { TimeSeriesChart } from "./TimeSeriesChart.tsx";
import {
  fromLocalInput,
  INTERVAL_MS,
  PRESETS,
  type PresetId,
  presetDuration,
  type RangeChoice,
  resolveRange,
  toLocalInput,
} from "./time.ts";

const PRESET_LABELS: Readonly<Record<PresetId, MessageId>> = {
  "1h": "device.history.range.1h",
  "6h": "device.history.range.6h",
  "24h": "device.history.range.24h",
  "7d": "device.history.range.7d",
  "30d": "device.history.range.30d",
  "90d": "device.history.range.90d",
};

const CUSTOM = "custom";

/** The instant the queryable period starts at, when the series API refused a range wholly before it. */
function outsidePeriod(error: unknown): number | null {
  if (!(error instanceof ApiError) || error.body?.message_key !== "telemetry.outside_queryable_period") return null;
  const earliest = error.body.details?.earliest;
  const instant = typeof earliest === "string" ? Date.parse(earliest) : Number.NaN;
  return Number.isFinite(instant) ? instant : null;
}

export interface HistoryCardProps {
  deviceId: string;
  tenant: string;
  points: readonly PointInfo[];
  /** The point on the chart; null when the device has none. */
  point: string | null;
  onPointChange(key: string): void;
  range: RangeChoice;
  /** The instant a preset range ends at. */
  anchor: number;
  onRangeChange(choice: RangeChoice): void;
  /** The model is still loading, so the points are not known yet. */
  pointsPending: boolean;
}

/**
 * One data point's stored history (`GET /api/telemetry/v1/devices/{id}/series`) over a range the
 * user picks, in their time zone. The server picks the interval (at most 1500 buckets). A range
 * the queryable period does not cover, a range without readings and a text data point each say
 * why there is no line and what to do instead.
 */
export function HistoryCard(props: HistoryCardProps) {
  const { deviceId, tenant, points, point, range, anchor } = props;
  const api = useDeviceApi();
  const format = useDeviceFormat();
  const { t, timeZone } = format;
  const { from, to } = resolveRange(range, anchor);
  const info = points.find((candidate) => candidate.key === point);

  const series = useQuery({
    queryKey: seriesKey(tenant, deviceId, { point: point ?? "", from, to, tz: timeZone }),
    queryFn: ({ signal }) =>
      api.getSeries({ deviceId, point: point ?? "", from: new Date(from), to: new Date(to), tz: timeZone }, signal),
    enabled: point !== null,
    // While another range of the same point loads, the previous line stays (marked as updating);
    // another point's line would be mislabelled, so it does not.
    placeholderData: (previous) => (previous?.point === point && previous?.tz === timeZone ? previous : undefined),
  });

  return (
    <Card id="device-history" tabIndex={-1} aria-labelledby="device-history-title" className="outline-none">
      <CardHeader>
        <CardTitle id="device-history-title">{t("device.history.title")}</CardTitle>
        <CardDescription>{t("device.history.description", { zone: timeZone })}</CardDescription>
        <CardAction>
          {series.isFetching && !series.isPending ? (
            <span className="flex items-center gap-1.5 text-sm text-muted-foreground">
              <Spinner />
              {t("device.history.updating")}
            </span>
          ) : null}
        </CardAction>
      </CardHeader>
      <CardContent className="grid gap-5">
        {props.pointsPending ? (
          <Spinner label={t("device.history.loading")} />
        ) : point === null ? (
          <EmptyState icon={<ChartLineIcon />} title={t("device.history.no_points_title")}>
            <p>{t("device.history.no_points_body")}</p>
          </EmptyState>
        ) : (
          <>
            <Controls {...props} point={point} from={from} to={to} />
            <SeriesView
              series={series}
              format={format}
              pointKind={info?.dataType === "bool" ? "bool" : info?.dataType === "string" ? "string" : "number"}
              request={{ point, from, to }}
              onRangeChange={props.onRangeChange}
            />
          </>
        )}
      </CardContent>
    </Card>
  );
}

function Controls({
  points,
  point,
  onPointChange,
  range,
  onRangeChange,
  from,
  to,
}: HistoryCardProps & { point: string; from: number; to: number }) {
  const { t } = useI18n();
  // "Custom" chosen in the list opens the form for the range shown then; any range chosen after
  // that, here or by a button elsewhere on the card, closes it again.
  const rangeKey = range.kind === "preset" ? range.preset : `${range.from}-${range.to}`;
  const [editingFor, setEditingFor] = useState<string | null>(null);
  const custom = editingFor === rangeKey || range.kind === "custom";
  return (
    <div className="grid gap-4">
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField label={t("device.history.point")}>
          {(control) => (
            <NativeSelect {...control} value={point} onChange={(event) => onPointChange(event.target.value)}>
              {points.map((option) => (
                <option key={option.key} value={option.key}>
                  {option.key}
                </option>
              ))}
            </NativeSelect>
          )}
        </FormField>
        <FormField label={t("device.history.range")}>
          {(control) => (
            <NativeSelect
              {...control}
              value={custom ? CUSTOM : range.kind === "preset" ? range.preset : CUSTOM}
              onChange={(event) => {
                const value = event.target.value;
                if (value === CUSTOM) {
                  setEditingFor(rangeKey);
                  return;
                }
                const preset = PRESETS.find((candidate) => candidate.id === value);
                if (preset) {
                  setEditingFor(null);
                  onRangeChange({ kind: "preset", preset: preset.id });
                }
              }}
            >
              {PRESETS.map((preset) => (
                <option key={preset.id} value={preset.id}>
                  {t(PRESET_LABELS[preset.id])}
                </option>
              ))}
              <option value={CUSTOM}>{t("device.history.range.custom")}</option>
            </NativeSelect>
          )}
        </FormField>
      </div>
      {custom ? (
        // Keyed by the range: a range chosen elsewhere (the period notice's button) refills it.
        <CustomRangeForm key={`${from}-${to}`} from={from} to={to} onApply={onRangeChange} />
      ) : null}
    </div>
  );
}

/** Two wall-clock times in the user's time zone; the end must be after the start. */
function CustomRangeForm({ from, to, onApply }: { from: number; to: number; onApply(choice: RangeChoice): void }) {
  const format = useDeviceFormat();
  const { t, timeZone } = format;
  const [start, setStart] = useState(() => toLocalInput(from, timeZone));
  const [end, setEnd] = useState(() => toLocalInput(to, timeZone));
  const [errors, setErrors] = useState<{ start?: MessageId; end?: MessageId }>({});
  const hintId = useId();

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const startAt = fromLocalInput(start, timeZone);
    const endAt = fromLocalInput(end, timeZone);
    const next: { start?: MessageId; end?: MessageId } = {};
    if (startAt === null) next.start = "device.history.from_invalid";
    if (endAt === null) next.end = "device.history.to_invalid";
    else if (startAt !== null && endAt <= startAt) next.end = "device.history.order_invalid";
    setErrors(next);
    if (startAt !== null && endAt !== null && next.end === undefined) {
      onApply({ kind: "custom", from: startAt, to: endAt });
    }
  };

  return (
    <form
      noValidate
      onSubmit={submit}
      aria-describedby={hintId}
      className="grid items-start gap-4 rounded-lg border bg-muted/40 p-4 sm:grid-cols-[1fr_1fr_auto]"
    >
      <FormField label={t("device.history.from")} error={errors.start ? t(errors.start) : undefined}>
        {(control) => (
          <Input {...control} type="datetime-local" value={start} onChange={(event) => setStart(event.target.value)} />
        )}
      </FormField>
      <FormField label={t("device.history.to")} error={errors.end ? t(errors.end) : undefined}>
        {(control) => (
          <Input {...control} type="datetime-local" value={end} onChange={(event) => setEnd(event.target.value)} />
        )}
      </FormField>
      <Button type="submit" className="sm:mt-[1.375rem]">
        <CalendarClockIcon />
        {t("device.history.apply")}
      </Button>
      <p id={hintId} className="text-sm text-muted-foreground sm:col-span-3">
        {t("device.history.custom_hint", { zone: timeZone })}
      </p>
    </form>
  );
}

function SeriesView({
  series,
  format,
  pointKind,
  request,
  onRangeChange,
}: {
  series: UseQueryResult<Series>;
  format: DeviceFormat;
  pointKind: "number" | "bool" | "string";
  request: { point: string; from: number; to: number };
  onRangeChange(choice: RangeChoice): void;
}) {
  const { t, errorMessage } = useI18n();

  if (series.isPending) {
    return <Spinner label={t("device.history.loading")} />;
  }
  if (series.isError) {
    const earliest = outsidePeriod(series.error);
    if (earliest !== null) {
      return (
        <EmptyState
          icon={<CalendarXIcon />}
          title={t("device.history.outside_title")}
          action={
            <Button variant="outline" onClick={() => onRangeChange({ kind: "custom", from: earliest, to: Date.now() })}>
              {t("device.history.outside_action", { earliest: format.minute(earliest) })}
            </Button>
          }
        >
          <p>
            {t("device.history.outside_body", {
              earliest: format.minute(earliest),
              from: format.minute(request.from),
              to: format.minute(request.to),
            })}
          </p>
        </EmptyState>
      );
    }
    return (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("device.history.failed", { reason: errorMessage(series.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void series.refetch()} disabled={series.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }

  const answer = series.data;
  const data = chartData(answer.points);
  const answerFrom = Date.parse(answer.from);
  const answerTo = Date.parse(answer.to);
  const truncated = answer.truncated ? (
    <Alert role="status">
      <CalendarClockIcon />
      <AlertDescription>
        <p>{t("device.history.truncated", { earliest: format.minute(answer.earliest) })}</p>
      </AlertDescription>
    </Alert>
  ) : null;

  if (answer.points.length === 0) {
    const longer = request.to - request.from < presetDuration("7d");
    return (
      <>
        {truncated}
        <EmptyState
          icon={<InboxIcon />}
          title={t("device.history.empty_title")}
          action={
            longer ? (
              <Button variant="outline" onClick={() => onRangeChange({ kind: "preset", preset: "7d" })}>
                {t("device.history.show_7d")}
              </Button>
            ) : undefined
          }
        >
          <p>
            {t("device.history.empty_body", {
              point: answer.point,
              from: format.minute(answerFrom),
              to: format.minute(answerTo),
            })}
          </p>
        </EmptyState>
      </>
    );
  }

  const kind = pointKind === "bool" || answer.points.every((bucket) => bucket.last.type === "bool") ? "bool" : "number";
  return (
    <div className="grid gap-4" aria-busy={series.isFetching}>
      {truncated}
      {data.length === 0 ? (
        <Alert role="status">
          <TypeIcon />
          <AlertDescription>
            <p>{t("device.history.text_only", { point: answer.point })}</p>
          </AlertDescription>
        </Alert>
      ) : (
        <ChartBlock data={data} answer={answer} from={answerFrom} to={answerTo} kind={kind} format={format} />
      )}
      <BucketTable answer={answer} format={format} initiallyOpen={data.length === 0} />
    </div>
  );
}

function ChartBlock({
  data,
  answer,
  from,
  to,
  kind,
  format,
}: {
  data: readonly Datum[];
  answer: Series;
  from: number;
  to: number;
  kind: "number" | "bool";
  format: DeviceFormat;
}) {
  const { t } = format;
  const stats = summary(data);
  const formatValue = useCallback(
    (value: number) => (kind === "bool" ? format.share(value) : format.number(value)),
    [kind, format],
  );
  const describe = useCallback(
    (datum: Datum) =>
      kind === "bool"
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
          }),
    [kind, format, t],
  );
  const facts =
    stats === null
      ? []
      : kind === "bool"
        ? [
            { label: t("device.history.on_share"), value: format.share(stats.mean) },
            { label: t("device.history.readings"), value: format.number(stats.count) },
          ]
        : [
            { label: t("device.history.min"), value: format.number(stats.min) },
            { label: t("device.history.max"), value: format.number(stats.max) },
            { label: t("device.history.mean"), value: format.number(stats.mean) },
            { label: t("device.history.readings"), value: format.number(stats.count) },
          ];
  return (
    <div className="grid gap-3">
      <dl className="flex flex-wrap gap-x-8 gap-y-2 text-sm">
        {facts.map((fact) => (
          <div key={fact.label} className="grid gap-0.5">
            <dt className="text-muted-foreground">{fact.label}</dt>
            <dd className="text-base font-semibold tabular-nums">{fact.value}</dd>
          </div>
        ))}
      </dl>
      <TimeSeriesChart
        data={data}
        from={from}
        to={to}
        intervalMs={INTERVAL_MS[answer.interval]}
        kind={kind}
        timeZone={format.timeZone}
        label={t("device.history.chart_label", {
          point: answer.point,
          from: format.minute(from),
          to: format.minute(to),
        })}
        formatValue={formatValue}
        formatTick={format.tick}
        describe={describe}
        hint={t("device.history.hint")}
      />
      <p className="text-sm text-muted-foreground">
        {t(kind === "bool" ? "device.history.interval_note_bool" : "device.history.interval_note", {
          interval: t(INTERVAL_LABELS[answer.interval]),
        })}
      </p>
    </div>
  );
}

/** The same buckets as a table: the chart's accessible version, and the only view of text readings. */
function BucketTable({
  answer,
  format,
  initiallyOpen,
}: {
  answer: Series;
  format: DeviceFormat;
  initiallyOpen: boolean;
}) {
  const { t } = format;
  const [open, setOpen] = useState(initiallyOpen);
  const tableId = useId();
  const cell = (value: number | null | undefined) =>
    typeof value === "number" && Number.isFinite(value) ? format.number(value) : "—";
  // Text readings have no mean, minimum or maximum: their table leaves those columns out.
  const stats = answer.points.some((bucket) => [bucket.avg, bucket.min, bucket.max].some((v) => typeof v === "number"));
  return (
    <div className="grid justify-items-start gap-3">
      <Button variant="outline" size="sm" aria-expanded={open} aria-controls={tableId} onClick={() => setOpen(!open)}>
        <TableIcon />
        {open ? t("device.history.hide_table") : t("device.history.show_table", { count: answer.points.length })}
      </Button>
      {open ? (
        <div id={tableId} className="max-h-96 w-full overflow-auto rounded-lg border">
          <table className="w-full text-sm tabular-nums">
            <caption className="sr-only">{t("device.history.table_caption", { point: answer.point })}</caption>
            <thead className="sticky top-0 bg-muted text-left text-muted-foreground">
              <tr>
                <th scope="col" className="px-3 py-2 font-medium">
                  {t("device.history.column.time")}
                </th>
                {stats ? (
                  <>
                    <th scope="col" className="px-3 py-2 text-right font-medium">
                      {t("device.history.mean")}
                    </th>
                    <th scope="col" className="px-3 py-2 text-right font-medium">
                      {t("device.history.min")}
                    </th>
                    <th scope="col" className="px-3 py-2 text-right font-medium">
                      {t("device.history.max")}
                    </th>
                  </>
                ) : null}
                <th scope="col" className="px-3 py-2 text-right font-medium">
                  {t("device.history.column.last")}
                </th>
                <th scope="col" className="px-3 py-2 text-right font-medium">
                  {t("device.history.readings")}
                </th>
              </tr>
            </thead>
            <tbody>
              {answer.points.map((bucket) => (
                <tr key={bucket.time} className="border-t">
                  <th scope="row" className="px-3 py-1.5 text-left font-normal whitespace-nowrap">
                    <time dateTime={bucket.time}>{format.minute(bucket.time)}</time>
                  </th>
                  {stats ? (
                    <>
                      <td className="px-3 py-1.5 text-right">{cell(bucket.avg)}</td>
                      <td className="px-3 py-1.5 text-right">{cell(bucket.min)}</td>
                      <td className="px-3 py-1.5 text-right">{cell(bucket.max)}</td>
                    </>
                  ) : null}
                  <td className="max-w-48 truncate px-3 py-1.5 text-right" title={format.reading(bucket.last)}>
                    {format.reading(bucket.last)}
                  </td>
                  <td className="px-3 py-1.5 text-right">{format.number(bucket.count)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}
    </div>
  );
}
