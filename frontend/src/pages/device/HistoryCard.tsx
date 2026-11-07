import { type UseQueryResult, useQuery } from "@tanstack/react-query";
import {
  CalendarClockIcon,
  CalendarIcon,
  CalendarXIcon,
  ChartLineIcon,
  InboxIcon,
  InfoIcon,
  TableIcon,
} from "lucide-react";
import { type FormEvent, type ReactNode, useEffect, useId, useMemo, useRef, useState } from "react";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { Series, SeriesPoint } from "../../api/devices.ts";
import { ApiError } from "../../api/errors.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { Card, CardAction, CardHeader, CardTitle } from "../../components/ui/card.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { useMediaQuery } from "../../lib/use-media-query.ts";
import { cn } from "../../lib/utils.ts";
import {
  boolSummary,
  chartData,
  type Datum,
  type Gap,
  gapsOf,
  gapTotal,
  within as inWindow,
  periods,
  summary,
} from "./chart.ts";
import { DeviceStretchAsk } from "./DeviceAsk.tsx";
import { type DeviceFormat, INTERVAL_LABELS, useDeviceFormat } from "./format.ts";
import { HistoryChart } from "./HistoryChart.tsx";
import type { PointInfo } from "./points.ts";
import { seriesKey } from "./queries.ts";
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
import { CLOCK_TICK_MS, useNow } from "./useNow.ts";

const PRESET_LABELS: Readonly<Record<PresetId, MessageId>> = {
  "1h": "device.history.range.1h",
  "6h": "device.history.range.6h",
  "24h": "device.history.range.24h",
  "7d": "device.history.range.7d",
  "30d": "device.history.range.30d",
  "90d": "device.history.range.90d",
};

const PRESET_SHORT_LABELS: Readonly<Record<PresetId, MessageId>> = {
  "1h": "device.history.preset.1h",
  "6h": "device.history.preset.6h",
  "24h": "device.history.preset.24h",
  "7d": "device.history.preset.7d",
  "30d": "device.history.preset.30d",
  "90d": "device.history.preset.90d",
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
  /** What 「分析這段」 calls the device. */
  deviceName: string;
  tenant: string;
  points: readonly PointInfo[];
  /** The point on the chart; null when the device has none. */
  point: string | null;
  range: RangeChoice;
  /** The instant a preset range ends at. */
  anchor: number;
  onRangeChange(choice: RangeChoice): void;
  /** The model is still loading, so the points are not known yet. */
  pointsPending: boolean;
  /**
   * The series loads again on the page's own timer, not because someone asked: no "updating"
   * text and no busy state; the line stays until the new one arrives.
   */
  quiet?: boolean;
  /** The chart is zoomed in (`true`) or back to the full range; the page does not refresh while it is zoomed. */
  onZoomChange?(zoomed: boolean): void;
  /** The series is loading (`true`) or is not. */
  onFetchingChange?(fetching: boolean): void;
}

/**
 * One data point's stored history (`GET /api/telemetry/v1/devices/{id}/series`) over a range the
 * user picks, in their time zone, as docs/design/device-page/ draws it. The server picks the
 * interval (at most 1500 buckets). A range the queryable period does not cover, a range without
 * readings and a text data point each say why there is no line and what to do instead.
 *
 * 「分析這段」 is the zoomed chart's (DeviceStretchAsk); 「跟上週比」 and 「為什麼離線？」 are the
 * page head's (DeviceAsk; Laysi/trustiot-v2#158).
 */
export function HistoryCard(props: HistoryCardProps) {
  const { deviceId, tenant, points, point, range, anchor, quiet = false } = props;
  const api = useDeviceApi();
  const format = useDeviceFormat();
  const { t, timeZone } = format;
  const { from, to } = resolveRange(range, anchor);
  const info = points.find((candidate) => candidate.key === point);
  const [formOpen, setFormOpen] = useState(false);

  const series = useQuery({
    queryKey: seriesKey(tenant, deviceId, { point: point ?? "", from, to, tz: timeZone }),
    queryFn: ({ signal }) =>
      api.getSeries({ deviceId, point: point ?? "", from: new Date(from), to: new Date(to), tz: timeZone }, signal),
    enabled: point !== null,
    // While another range of the same point loads, the previous line stays (marked as updating);
    // another point's line would be mislabelled, so it does not.
    placeholderData: (previous) => (previous?.point === point && previous?.tz === timeZone ? previous : undefined),
  });
  const title = info?.description ?? point ?? "";
  // A range's identity, not its instants: a preset's end moves on every refresh, and the chart
  // must stay the same one (its focus, tooltip and table) while it does.
  const rangeKey = range.kind === "preset" ? range.preset : `${range.from}-${range.to}`;
  // The newest answer that came back, of this data point, zone and range: the series of a new end is
  // a new query, so when a refresh of the same range fails there is nothing under its key, and the
  // line on screen is this one. Another range's answer is never kept: it would be mislabelled.
  const lastAnswer = useRef<{ answer: Series; rangeKey: string } | undefined>(undefined);
  useEffect(() => {
    if (series.data !== undefined && !series.isPlaceholderData) lastAnswer.current = { answer: series.data, rangeKey };
  }, [series.data, series.isPlaceholderData, rangeKey]);
  const last = lastAnswer.current;
  const kept =
    series.isError && last?.answer.point === point && last.answer.tz === timeZone && last.rangeKey === rangeKey
      ? last.answer
      : undefined;
  const { onFetchingChange } = props;
  // The page learns when the series is loading, to end the quiet of an automatic refresh once it is done.
  useEffect(() => {
    onFetchingChange?.(series.isFetching);
    return () => onFetchingChange?.(false);
  }, [series.isFetching, onFetchingChange]);

  return (
    <Card
      id="device-history"
      tabIndex={-1}
      aria-labelledby="device-history-title"
      aria-busy={series.isPending && point !== null}
      className="gap-4 py-0 outline-none"
    >
      <CardHeader className="pt-5">
        <div className="grid min-w-0 gap-0.5">
          <CardTitle id="device-history-title" className="text-base wrap-anywhere">
            {point === null ? t("device.history.title_none") : t("device.history.title", { point: title })}
          </CardTitle>
          {point !== null ? <p className="font-mono text-xs text-muted-foreground wrap-anywhere">{point}</p> : null}
        </div>
        <CardAction>
          {series.isFetching && !series.isPending && !quiet ? (
            <span className="flex items-center gap-1.5 text-sm text-muted-foreground">
              <Spinner />
              {t("device.history.updating")}
            </span>
          ) : null}
        </CardAction>
      </CardHeader>
      {point === null && !props.pointsPending ? (
        <EmptyState centered icon={<ChartLineIcon />} title={t("device.history.no_points_title")}>
          <p>{t("device.history.no_points_body")}</p>
        </EmptyState>
      ) : (
        <>
          <Toolbar
            range={range}
            from={from}
            to={to}
            format={format}
            formOpen={formOpen}
            onFormOpen={setFormOpen}
            onRangeChange={props.onRangeChange}
            earliest={series.data?.earliest}
          />
          {props.pointsPending ? (
            <ChartSkeleton label={t("device.history.loading")} />
          ) : (
            <DeviceStretchAsk deviceName={props.deviceName} point={title}>
              <SeriesBody
                series={series}
                format={format}
                title={title}
                pointKind={info?.dataType === "bool" ? "bool" : info?.dataType === "string" ? "string" : "number"}
                request={{ point: point ?? "", from, to }}
                rangeKey={rangeKey}
                kept={kept}
                quiet={quiet}
                onRangeChange={props.onRangeChange}
                onZoomChange={props.onZoomChange}
              />
            </DeviceStretchAsk>
          )}
        </>
      )}
    </Card>
  );
}

/** The range: segmented buttons on a wide screen, the browser's own list on a phone (seven options do not fit), and the custom form. */
function Toolbar({
  range,
  from,
  to,
  format,
  formOpen,
  onFormOpen,
  onRangeChange,
  earliest,
}: {
  range: RangeChoice;
  from: number;
  to: number;
  format: DeviceFormat;
  formOpen: boolean;
  onFormOpen(open: boolean): void;
  onRangeChange(choice: RangeChoice): void;
  earliest: string | undefined;
}) {
  const { t } = format;
  const wide = useMediaQuery("(min-width: 640px)");
  const isCustom = range.kind === "custom" || formOpen;
  const choose = (preset: PresetId) => {
    onFormOpen(false);
    onRangeChange({ kind: "preset", preset });
  };
  return (
    <div className="grid gap-3 px-5 sm:px-6">
      <div className="flex flex-wrap items-center justify-between gap-2.5">
        {wide ? (
          <fieldset className="m-0 inline-flex min-w-0 gap-0.5 rounded-[10px] border-0 bg-muted p-[3px]">
            <legend className="sr-only">{t("device.history.range")}</legend>
            {PRESETS.map((preset) => (
              <SegmentButton
                key={preset.id}
                pressed={!isCustom && range.kind === "preset" && range.preset === preset.id}
                onClick={() => choose(preset.id)}
              >
                {t(PRESET_SHORT_LABELS[preset.id])}
              </SegmentButton>
            ))}
            <SegmentButton pressed={isCustom} onClick={() => onFormOpen(!formOpen)}>
              <CalendarIcon className="size-3.5" aria-hidden="true" />
              {t("device.history.custom_short")}
            </SegmentButton>
          </fieldset>
        ) : (
          <FormField label={t("device.history.range")} className="w-full">
            {(control) => (
              <NativeSelect
                {...control}
                value={isCustom ? CUSTOM : range.kind === "preset" ? range.preset : CUSTOM}
                onChange={(event) => {
                  const value = event.target.value;
                  const preset = PRESETS.find((candidate) => candidate.id === value);
                  if (preset) choose(preset.id);
                  else onFormOpen(true);
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
        )}
        {range.kind === "custom" ? (
          <p className="flex items-center gap-2 text-sm text-muted-foreground tabular-nums">
            <span>
              {format.minute(from)} – {format.minute(to)}
            </span>
            {/* The select already shows 「custom」, so choosing it again fires nothing: a button opens the form. */}
            {!wide && !formOpen ? (
              <Button variant="ghost" size="sm" onClick={() => onFormOpen(true)}>
                {t("device.history.edit_range")}
              </Button>
            ) : null}
          </p>
        ) : null}
      </div>
      {formOpen ? (
        // Keyed by the range: a range chosen elsewhere (the period notice's button) refills it.
        <CustomRangeForm
          key={`${from}-${to}`}
          from={from}
          to={to}
          earliest={earliest}
          onApply={(choice) => {
            onFormOpen(false);
            onRangeChange(choice);
          }}
          onCancel={() => onFormOpen(false)}
        />
      ) : null}
    </div>
  );
}

function SegmentButton({ pressed, onClick, children }: { pressed: boolean; onClick(): void; children: ReactNode }) {
  return (
    <button
      type="button"
      aria-pressed={pressed}
      onClick={onClick}
      className={cn(
        "inline-flex items-center gap-1.5 rounded-[7px] px-2.5 py-1.5 text-[13px] whitespace-nowrap text-muted-foreground",
        focusRing,
        pressed && "bg-background font-semibold text-foreground shadow-xs",
      )}
    >
      {children}
    </button>
  );
}

/** Two wall-clock times in the user's time zone; the end must be after the start. */
function CustomRangeForm({
  from,
  to,
  earliest,
  onApply,
  onCancel,
}: {
  from: number;
  to: number;
  earliest: string | undefined;
  onApply(choice: RangeChoice): void;
  onCancel(): void;
}) {
  const format = useDeviceFormat();
  const { t, timeZone } = format;
  const [start, setStart] = useState(() => toLocalInput(from, timeZone));
  const [end, setEnd] = useState(() => toLocalInput(to, timeZone));
  const [errors, setErrors] = useState<{ start?: MessageId; end?: MessageId }>({});
  const hintId = useId();
  const titleId = useId();

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
      aria-labelledby={titleId}
      aria-describedby={hintId}
      className="grid gap-3 rounded-xl border p-3.5"
    >
      <p id={titleId} className="text-sm font-semibold">
        {t("device.history.custom_title")}
      </p>
      <div className="grid items-start gap-3 sm:grid-cols-2">
        <FormField label={t("device.history.from")} error={errors.start ? t(errors.start) : undefined}>
          {(control) => (
            <Input
              {...control}
              type="datetime-local"
              value={start}
              onChange={(event) => setStart(event.target.value)}
            />
          )}
        </FormField>
        <FormField label={t("device.history.to")} error={errors.end ? t(errors.end) : undefined}>
          {(control) => (
            <Input {...control} type="datetime-local" value={end} onChange={(event) => setEnd(event.target.value)} />
          )}
        </FormField>
      </div>
      <p id={hintId} className="text-xs text-muted-foreground">
        {t("device.history.custom_hint", { zone: timeZone })}
        {earliest !== undefined ? ` ${t("device.history.custom_limit", { earliest: format.minute(earliest) })}` : ""}
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <Button type="submit" size="sm">
          <CalendarClockIcon />
          {t("device.history.apply")}
        </Button>
        <Button type="button" variant="ghost" size="sm" onClick={onCancel}>
          {t("device.history.cancel")}
        </Button>
      </div>
    </form>
  );
}

function ChartSkeleton({ label }: { label: string }) {
  return (
    <div role="status" className="px-5 pb-5 sm:px-6">
      <div aria-hidden="true" className="h-56 animate-pulse rounded-lg bg-muted sm:h-80" />
      <p className="mt-2.5 text-sm text-muted-foreground">{label}</p>
    </div>
  );
}

function Note({ icon, children }: { icon: ReactNode; children: ReactNode }) {
  return (
    <div className="px-5 sm:px-6">
      <Alert role="status" className="bg-muted text-foreground [&>svg]:text-muted-foreground">
        {icon}
        <AlertDescription className="text-foreground">{children}</AlertDescription>
      </Alert>
    </div>
  );
}

function SeriesBody({
  series,
  format,
  title,
  pointKind,
  request,
  rangeKey,
  kept,
  quiet,
  onRangeChange,
  onZoomChange,
}: {
  series: UseQueryResult<Series>;
  format: DeviceFormat;
  title: string;
  pointKind: "number" | "bool" | "string";
  request: { point: string; from: number; to: number };
  rangeKey: string;
  /** The answer on screen when a refresh of its range failed; it stays, with the failure above it. */
  kept: Series | undefined;
  quiet: boolean;
  onRangeChange(choice: RangeChoice): void;
  onZoomChange: ((zoomed: boolean) => void) | undefined;
}) {
  const { t, errorMessage } = useI18n();
  // One shape for every answer, so the chart under it is the same element whether or not a failure
  // is shown above it: a chart that is rebuilt loses its focus, tooltip and table.
  const ready = (answer: Series, failure: ReactNode) => (
    <>
      {failure}
      <ReadyBody
        answer={answer}
        busy={series.isFetching && !quiet}
        format={format}
        title={title}
        pointKind={pointKind}
        request={request}
        rangeKey={rangeKey}
        onRangeChange={onRangeChange}
        onZoomChange={onZoomChange}
        updatedAt={series.dataUpdatedAt}
      />
    </>
  );
  const failed = (error: unknown) => (
    <div className="px-5 pb-5 sm:px-6">
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("device.history.failed", { reason: errorMessage(error) })}</p>
        </AlertDescription>
        <Button variant="outline" size="sm" onClick={() => void series.refetch()} disabled={series.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    </div>
  );

  if (series.isPending) {
    return <ChartSkeleton label={t("device.history.loading")} />;
  }
  if (series.isError) {
    const earliest = outsidePeriod(series.error);
    if (kept !== undefined && earliest === null) return ready(kept, failed(series.error));
    if (earliest !== null) {
      return (
        <EmptyState
          centered
          icon={<CalendarXIcon />}
          title={t("device.history.outside_title")}
          action={
            <Button onClick={() => onRangeChange({ kind: "custom", from: earliest, to: Date.now() })}>
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
    return failed(series.error);
  }
  return ready(series.data, null);
}

function ReadyBody({
  answer,
  busy,
  format,
  title,
  pointKind,
  request,
  rangeKey,
  onRangeChange,
  onZoomChange,
  updatedAt,
}: {
  answer: Series;
  busy: boolean;
  format: DeviceFormat;
  title: string;
  pointKind: "number" | "bool" | "string";
  request: { point: string; from: number; to: number };
  rangeKey: string;
  onRangeChange(choice: RangeChoice): void;
  onZoomChange: ((zoomed: boolean) => void) | undefined;
  updatedAt: number;
}) {
  const { t } = format;
  // One list per answer: a new one every render would rebuild the chart's option, which puts a
  // zoomed chart back to the full range (the clock's tick re-renders the page every 30 seconds).
  const data = useMemo(() => chartData(answer.points), [answer.points]);
  const answerFrom = Date.parse(answer.from);
  const answerTo = Date.parse(answer.to);
  const truncated = answer.truncated ? (
    <Note icon={<InfoIcon aria-hidden="true" />}>
      <p>{t("device.history.truncated", { earliest: format.minute(answer.earliest) })}</p>
    </Note>
  ) : null;

  if (answer.points.length === 0) {
    const longer = request.to - request.from < presetDuration("7d");
    return (
      <>
        {truncated}
        <EmptyState
          centered
          icon={<InboxIcon />}
          title={t("device.history.empty_title")}
          action={
            longer ? (
              <Button onClick={() => onRangeChange({ kind: "preset", preset: "7d" })}>
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

  // Text has no number to plot: the periods table says what there is.
  if (data.length === 0) {
    return (
      <>
        {truncated}
        <TextPeriods answer={answer} format={format} />
      </>
    );
  }
  const kind = pointKind === "bool" || answer.points.every((bucket) => bucket.last.type === "bool") ? "bool" : "number";
  return (
    <ChartBlock
      key={`${answer.point}-${rangeKey}`}
      answer={answer}
      busy={busy}
      data={data}
      kind={kind}
      from={answerFrom}
      to={answerTo}
      title={title}
      format={format}
      truncated={truncated}
      onRangeChange={onRangeChange}
      onZoomChange={onZoomChange}
      updatedAt={updatedAt}
    />
  );
}

/** A text point is not drawn: the stretches of one value, newest first. */
function TextPeriods({ answer, format }: { answer: Series; format: DeviceFormat }) {
  const { t } = format;
  const intervalMs = INTERVAL_MS[answer.interval];
  const rows = useMemo(() => periods(answer.points, intervalMs), [answer.points, intervalMs]);
  return (
    <>
      <Note icon={<InfoIcon aria-hidden="true" />}>
        <p>{t("device.history.text_note", { point: answer.point })}</p>
      </Note>
      <div className="overflow-x-auto px-5 pb-5 sm:px-6">
        <table className="w-full text-sm">
          <caption className="pb-2 text-left text-[13px] text-muted-foreground">
            {t("device.history.period_caption", { point: answer.point })}
          </caption>
          <thead>
            <tr className="border-b text-left text-[13px] text-muted-foreground">
              <th scope="col" className="px-2.5 py-2 font-medium">
                {t("device.history.column.value")}
              </th>
              <th scope="col" className="px-2.5 py-2 font-medium">
                {t("device.history.column.from")}
              </th>
              <th scope="col" className="px-2.5 py-2 font-medium">
                {t("device.history.column.to")}
              </th>
              <th scope="col" className="px-2.5 py-2 text-right font-medium">
                {t("device.history.column.buckets")}
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={row.from} className="border-b last:border-b-0">
                <td className="max-w-48 truncate px-2.5 py-2 font-mono" title={row.value}>
                  {row.value}
                </td>
                <td className="px-2.5 py-2 whitespace-nowrap tabular-nums">
                  <time dateTime={new Date(row.from).toISOString()}>{format.minute(row.from)}</time>
                </td>
                <td className="px-2.5 py-2 whitespace-nowrap tabular-nums">
                  <time dateTime={new Date(row.to).toISOString()}>{format.minute(row.to)}</time>
                </td>
                <td className="px-2.5 py-2 text-right tabular-nums">{format.number(row.buckets)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}

function ChartBlock({
  answer,
  busy,
  data,
  kind,
  from,
  to,
  title,
  format,
  truncated,
  onRangeChange,
  onZoomChange,
  updatedAt,
}: {
  answer: Series;
  busy: boolean;
  data: Datum[];
  kind: "number" | "bool";
  from: number;
  to: number;
  title: string;
  format: DeviceFormat;
  truncated: ReactNode;
  onRangeChange(choice: RangeChoice): void;
  onZoomChange: ((zoomed: boolean) => void) | undefined;
  /** When the answer arrived: the chart's "now" is never before it (the page does the same for the status). */
  updatedAt: number;
}) {
  const { t } = format;
  const intervalMs = INTERVAL_MS[answer.interval];
  const [zoom, setZoom] = useState<{ from: number; to: number } | null>(null);
  const zoomed = zoom !== null;
  // The page pauses its own refresh while the chart is zoomed; a chart that goes away is not zoomed.
  useEffect(() => {
    onZoomChange?.(zoomed);
    return () => onZoomChange?.(false);
  }, [zoomed, onZoomChange]);
  const [tableOpen, setTableOpen] = useState(false);
  // A newest bucket is open until its interval ends, and a range may end in the future: gaps and
  // their total stop at the present.
  const tick = useNow(CLOCK_TICK_MS);
  const now = Math.max(tick, updatedAt);
  const end = useMemo(() => Math.min(to, Math.max(from, now)), [from, to, now]);
  const gaps: Gap[] = useMemo(() => gapsOf(data, intervalMs, from, end), [data, intervalMs, from, end]);
  const span = zoom ?? { from, to: end };
  const visible = zoom ? inWindow(data, zoom.from, zoom.to) : data;
  const boolBuckets = zoom
    ? answer.points.filter((bucket) => {
        const time = Date.parse(bucket.time);
        return time >= zoom.from && time < zoom.to;
      })
    : answer.points;
  const missing = gapTotal(gaps, span.from, span.to);

  const stats =
    kind === "bool" ? boolStats(boolBuckets, format) : numberStats(summary(visible), zoom !== null, missing, format);
  const interval = t(INTERVAL_LABELS[answer.interval]);

  return (
    <div className="grid gap-4 pb-5" aria-busy={busy}>
      {truncated}
      <dl
        className="grid grid-cols-2 gap-x-3 gap-y-2.5 px-5 sm:grid-cols-4 sm:gap-x-3 sm:px-6"
        aria-label={zoom ? t("device.history.stats_zoomed") : undefined}
      >
        {stats.map((stat) => (
          <div key={stat.label} className="grid min-w-0 content-start gap-0.5">
            <dt className="text-xs text-muted-foreground">{stat.label}</dt>
            <dd className="text-lg leading-snug font-semibold tabular-nums">
              {stat.value}
              {stat.note ? <span className="block text-xs font-normal text-muted-foreground">{stat.note}</span> : null}
            </dd>
          </div>
        ))}
      </dl>
      <div className="px-3 sm:px-4">
        <HistoryChart
          data={data}
          kind={kind}
          gaps={gaps}
          from={from}
          to={to}
          title={title}
          label={t("device.history.chart_label", {
            point: answer.point,
            from: format.minute(from),
            to: format.minute(to),
          })}
          format={format}
          onZoom={setZoom}
          onSetRange={(chosen) => onRangeChange({ kind: "custom", from: chosen.from, to: chosen.to })}
          onOpenTable={() => setTableOpen(true)}
        />
      </div>
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 px-5 sm:px-6">
        <ul className="m-0 flex list-none flex-wrap items-center gap-x-3.5 gap-y-1 p-0 text-xs text-muted-foreground">
          <li className="flex items-center gap-1.5">
            <span aria-hidden="true" className="inline-block h-0 w-3.5 border-t-2 border-chart-1" />
            {t(kind === "bool" ? "device.history.legend_on_share" : "device.history.legend_mean", { interval })}
          </li>
          {kind === "number" ? (
            <li className="flex items-center gap-1.5">
              <span aria-hidden="true" className="inline-block h-2.5 w-3.5 rounded-xs bg-chart-1/20" />
              {t("device.history.legend_band")}
            </li>
          ) : null}
          <li className="flex items-center gap-1.5">
            <span
              aria-hidden="true"
              className="inline-block h-2.5 w-3.5 rounded-xs border border-dashed border-input"
            />
            {t("device.history.legend_gap")}
          </li>
          <li>{t("device.history.zone", { zone: format.timeZone })}</li>
        </ul>
        <TableToggle
          open={tableOpen}
          count={answer.points.length}
          onToggle={() => setTableOpen(!tableOpen)}
          format={format}
        />
      </div>
      {tableOpen ? <BucketTable answer={answer} format={format} /> : null}
    </div>
  );
}

interface Stat {
  label: string;
  value: string;
  note?: string;
}

function numberStats(
  figures: ReturnType<typeof summary>,
  zoomed: boolean,
  missing: number,
  format: DeviceFormat,
): Stat[] {
  const { t } = format;
  if (figures === null) return [];
  const minutes = Math.round(missing / 60_000) * 60_000;
  return [
    { label: t(zoomed ? "device.history.mean_zoomed" : "device.history.mean"), value: format.number(figures.mean) },
    { label: t("device.history.min"), value: format.number(figures.min.value), note: format.minute(figures.min.time) },
    { label: t("device.history.max"), value: format.number(figures.max.value), note: format.minute(figures.max.time) },
    {
      label: t("device.history.readings"),
      value: t("device.history.readings_count", { count: figures.count }),
      note:
        minutes > 0
          ? t("device.history.gap_label", { duration: format.elapsed(minutes) })
          : t("device.history.no_gaps"),
    },
  ];
}

function boolStats(buckets: readonly SeriesPoint[], format: DeviceFormat): Stat[] {
  const { t } = format;
  const figures = boolSummary(buckets);
  return [
    {
      label: t("device.history.state_now"),
      value: figures.current === null ? "—" : t(figures.current ? "device.value.on" : "device.value.off"),
      ...(figures.since !== null ? { note: t("device.history.since", { time: format.minute(figures.since) }) } : {}),
    },
    { label: t("device.history.on_share"), value: figures.share === null ? "—" : format.share(figures.share) },
    { label: t("device.history.turned_on"), value: t("device.history.times", { count: figures.turnedOn }) },
    { label: t("device.history.readings"), value: t("device.history.readings_count", { count: figures.readings }) },
  ];
}

function TableToggle({
  open,
  count,
  onToggle,
  format,
}: {
  open: boolean;
  count: number;
  onToggle(): void;
  format: DeviceFormat;
}) {
  const { t } = format;
  return (
    <Button variant="ghost" size="sm" aria-expanded={open} aria-controls="device-history-table" onClick={onToggle}>
      <TableIcon />
      {open ? t("device.history.hide_table") : t("device.history.show_table", { count })}
    </Button>
  );
}

/** The same buckets as a table: the chart's accessible version. */
function BucketTable({ answer, format }: { answer: Series; format: DeviceFormat }) {
  const { t } = format;
  const cell = (value: number | null | undefined) =>
    typeof value === "number" && Number.isFinite(value) ? format.number(value) : "—";
  return (
    <div id="device-history-table" className="mx-5 max-h-96 overflow-auto rounded-lg border sm:mx-6">
      <table className="w-full text-sm tabular-nums">
        <caption className="sr-only">{t("device.history.table_caption", { point: answer.point })}</caption>
        <thead className="sticky top-0 bg-muted text-left text-muted-foreground">
          <tr>
            <th scope="col" className="px-3 py-2 font-medium">
              {t("device.history.column.time")}
            </th>
            <th scope="col" className="px-3 py-2 text-right font-medium">
              {t("device.history.mean")}
            </th>
            <th scope="col" className="px-3 py-2 text-right font-medium">
              {t("device.history.min")}
            </th>
            <th scope="col" className="px-3 py-2 text-right font-medium">
              {t("device.history.max")}
            </th>
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
              <td className="px-3 py-1.5 text-right">{cell(bucket.avg)}</td>
              <td className="px-3 py-1.5 text-right">{cell(bucket.min)}</td>
              <td className="px-3 py-1.5 text-right">{cell(bucket.max)}</td>
              <td className="max-w-48 truncate px-3 py-1.5 text-right" title={format.reading(bucket.last)}>
                {format.reading(bucket.last)}
              </td>
              <td className="px-3 py-1.5 text-right">{format.number(bucket.count)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
