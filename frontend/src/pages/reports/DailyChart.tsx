import { useId } from "react";

import type { ReportDay } from "../../api/pending/reports.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { formatAmount } from "./format.ts";
import { monthDay } from "./schedule-time.ts";

const WIDTH = 600;
const HEIGHT = 200;
const AXIS = 22;

/**
 * The daily energy as bars, in SVG (decision Q8: no chart library for a few dozen bars; the bar
 * positions are attributes, never `style`, for the CSP). It is an image for assistive technology
 * with a one-line summary; the table next to it (`DailyTable`) has every value.
 */
export function DailyChart({ days }: { days: readonly ReportDay[] }) {
  const { t, locale } = useI18n();
  const titleId = useId();
  if (days.length === 0) return null;
  const max = Math.max(...days.map((day) => day.kwh), 0);
  const highest = days.reduce((best, day) => (day.kwh > best.kwh ? day : best), days[0] as ReportDay);
  const lowest = days.reduce((best, day) => (day.kwh < best.kwh ? day : best), days[0] as ReportDay);
  const slot = WIDTH / days.length;
  const barWidth = Math.max(slot * 0.7, 1);
  const label = t("reports.daily.chart_label", {
    max: formatAmount(highest.kwh, locale),
    maxDate: monthDay(highest.date),
    min: formatAmount(lowest.kwh, locale),
    minDate: monthDay(lowest.date),
  });
  const middle = days[Math.floor((days.length - 1) / 2)] as ReportDay;
  return (
    <svg
      viewBox={`0 0 ${WIDTH} ${HEIGHT + AXIS}`}
      role="img"
      aria-labelledby={titleId}
      className="h-auto w-full max-w-full"
    >
      <title id={titleId}>{label}</title>
      <line x1={0} x2={WIDTH} y1={HEIGHT} y2={HEIGHT} className="stroke-border" strokeWidth={1} />
      {days.map((day, index) => {
        const height = max > 0 ? (day.kwh / max) * (HEIGHT - 4) : 0;
        return (
          <rect
            key={day.date}
            x={index * slot + (slot - barWidth) / 2}
            y={HEIGHT - height}
            width={barWidth}
            height={height}
            rx={1.5}
            className="fill-primary"
          >
            <title>{`${monthDay(day.date)}: ${formatAmount(day.kwh, locale)} kWh`}</title>
          </rect>
        );
      })}
      <g aria-hidden="true" className="fill-muted-foreground text-[11px]">
        <text x={0} y={HEIGHT + 15} textAnchor="start">
          {monthDay((days[0] as ReportDay).date)}
        </text>
        <text x={WIDTH / 2} y={HEIGHT + 15} textAnchor="middle">
          {monthDay(middle.date)}
        </text>
        <text x={WIDTH} y={HEIGHT + 15} textAnchor="end">
          {monthDay((days[days.length - 1] as ReportDay).date)}
        </text>
      </g>
    </svg>
  );
}

/** The same values as a table: the alternative to the picture, one row per day. */
export function DailyTable({ days }: { days: readonly ReportDay[] }) {
  const { t, locale } = useI18n();
  return (
    <div className="max-h-80 overflow-y-auto">
      <table className="w-full text-sm">
        <caption className="sr-only">{t("reports.daily.table_caption")}</caption>
        <thead className="text-left text-muted-foreground">
          <tr>
            <th scope="col" className="py-1.5 font-medium">
              {t("reports.daily.column.date")}
            </th>
            <th scope="col" className="py-1.5 text-right font-medium">
              {t("reports.daily.column.kwh")}
            </th>
          </tr>
        </thead>
        <tbody>
          {days.map((day) => (
            <tr key={day.date} className="border-t">
              <th scope="row" className="py-1.5 text-left font-normal tabular-nums">
                {day.date}
              </th>
              <td className="py-1.5 text-right tabular-nums">{formatAmount(day.kwh, locale)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
