import type { components } from "../api/schema.gen.ts";
import type { MessageId } from "../i18n/messages.ts";
import { type DashboardEnv, useDashboardEnv } from "./context.tsx";
import { useComponentExtras } from "./extensions.tsx";
import { NO_VALUE } from "./format.ts";

/** A statistics or usage query's request, as the dataset stores it. */
export type SemanticRequest = components["schemas"]["DashboardSemanticRequest"];

const RELATIVE_PERIODS: Readonly<Record<string, MessageId>> = {
  today: "dashboard.source.period_today",
  yesterday: "dashboard.source.period_yesterday",
  this_week: "dashboard.source.period_this_week",
  last_week: "dashboard.source.period_last_week",
  this_month: "dashboard.source.period_this_month",
  last_month: "dashboard.source.period_last_month",
  this_year: "dashboard.source.period_this_year",
  last_year: "dashboard.source.period_last_year",
};

const LAST_N = /^last_([1-9][0-9]{0,2})_(days|hours)$/;

/**
 * What the request's period says in words: a relative period as the viewer reads it, an explicit
 * one as its two instants in the viewer's time zone. A relative period this build has no words
 * for is shown as written, never guessed.
 */
export function periodText(request: SemanticRequest, env: Pick<DashboardEnv, "t" | "format">): string {
  const { t, format } = env;
  if (request.period != null) {
    const known = Object.hasOwn(RELATIVE_PERIODS, request.period) ? RELATIVE_PERIODS[request.period] : undefined;
    if (known !== undefined) return t(known);
    const last = LAST_N.exec(request.period);
    if (last !== null) {
      return t(last[2] === "days" ? "dashboard.source.period_last_days" : "dashboard.source.period_last_hours", {
        count: Number(last[1]),
      });
    }
    return request.period;
  }
  if (request.from != null && request.to != null) {
    return t("dashboard.source.period_range", { from: format.instant(request.from), to: format.instant(request.to) });
  }
  return NO_VALUE;
}

const INTERVALS: Readonly<Record<string, MessageId>> = {
  "15m": "dashboard.source.interval_15m",
  "1h": "dashboard.source.interval_1h",
  "1d": "dashboard.source.interval_1d",
  "1w": "dashboard.source.interval_1w",
  "1mo": "dashboard.source.interval_1mo",
  total: "dashboard.source.interval_total",
};

/**
 * The request in words, for a viewer who cannot read a JSON: the data points (a device by name,
 * by ID while the name is not known), the period, how the readings are grouped and the time zone.
 * The raw parameters stay in the panel next to it.
 */
export function RequestSummary({ request }: { request: SemanticRequest }) {
  const env = useDashboardEnv();
  const { t } = env;
  const { deviceName } = useComponentExtras();
  // The interval defaults to one bucket for the whole period (`SemanticInterval`).
  const key = request.interval ?? "total";
  const interval = Object.hasOwn(INTERVALS, key) ? INTERVALS[key] : undefined;
  return (
    <dl className="grid gap-1.5 text-sm" data-testid="request-summary">
      <div className="grid gap-0.5 sm:grid-cols-[6rem_1fr] sm:gap-3">
        <dt className="text-muted-foreground">{t("dashboard.source.series")}</dt>
        <dd className="min-w-0">
          <ul className="grid gap-0.5">
            {request.series.map((series) => (
              <li key={`${series.device_id}/${series.point}`} className="min-w-0 break-words">
                <span className="font-medium">{deviceName?.(series.device_id) ?? series.device_id}</span>
                <span aria-hidden="true"> · </span>
                <span className="sr-only">, </span>
                <span className="font-mono text-xs">{series.point}</span>
              </li>
            ))}
          </ul>
        </dd>
      </div>
      <div className="grid gap-0.5 sm:grid-cols-[6rem_1fr] sm:gap-3">
        <dt className="text-muted-foreground">{t("dashboard.source.period")}</dt>
        <dd className="min-w-0 break-words tabular-nums">{periodText(request, env)}</dd>
      </div>
      <div className="grid gap-0.5 sm:grid-cols-[6rem_1fr] sm:gap-3">
        <dt className="text-muted-foreground">{t("dashboard.source.interval")}</dt>
        <dd className="min-w-0">{interval === undefined ? key : t(interval)}</dd>
      </div>
      <div className="grid gap-0.5 sm:grid-cols-[6rem_1fr] sm:gap-3">
        <dt className="text-muted-foreground">{t("dashboard.source.time_zone")}</dt>
        <dd className="min-w-0 break-words">{request.tz}</dd>
      </div>
    </dl>
  );
}
