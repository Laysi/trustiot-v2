// How the components write values. Every number a component shows (a KPI, a tooltip, a table
// cell, a threshold's label) goes through `DashboardFormat`, so a test can format the data with
// the same function and compare it with the screen. Formatting only rounds for display: it never
// adds, averages or converts a value (docs/architecture/ai-assistant.md, 「兩條路的正確性」).
//
// Times in results are wall-clock times as the query wrote them (frontend/sandbox/README.md,
// 「時間」): a component keeps them as a `Date` whose UTC fields are that wall time and formats
// them in UTC, so the viewer sees the query's local time whatever their own zone is. Only the
// time a dataset ran (`queried_at`, an instant) is shown in the viewer's time zone.

import { parseWallTime } from "../../sandbox/src/time.ts";
import type { Locale } from "../i18n/messages.ts";
import { translate } from "../i18n/translate.ts";
import type { Cell, ColumnType } from "./data.ts";

/** What a missing value (`null`, NaN or infinity in the source) shows as. */
export const NO_VALUE = "—";

const DATE_ONLY = /^\d{4}-\d{2}-\d{2}$/;
const DAY_MS = 86_400_000;

export interface DashboardFormat {
  locale: Locale;
  /** A number as a component shows it: up to 3 decimals, or 3 significant digits below 1. */
  number(value: number | null): string;
  /** `text` followed by the unit, when there is one. */
  withUnit(text: string, unit: string | undefined): string;
  /** A wall-clock time from a result (milliseconds of its UTC fields), to the precision it has. */
  wallTime(ms: number, dateOnly?: boolean): string;
  /** A result cell by its column's type: a number, text as written, a time, yes or no. */
  cell(type: ColumnType, cell: Cell): string;
  /** A label on a time axis that spans `spanMs`: a position, not a data value. */
  axisTime(ms: number, spanMs: number): string;
  /** A label on a value axis: a position, not a data value. */
  axisNumber(value: number): string;
  /** An instant (RFC 3339, `queried_at`) in the viewer's time zone. */
  instant(text: string): string;
}

/** A time cell as milliseconds of its wall-clock fields, or `null` when it is not a time. */
export function wallMs(cell: Cell): number | null {
  if (typeof cell !== "string") return null;
  const date = parseWallTime(cell);
  return date === null ? null : date.getTime();
}

/** Whether a time cell holds only a date. */
export function isDateOnly(cell: Cell): boolean {
  return typeof cell === "string" && DATE_ONLY.test(cell);
}

export function dashboardFormat(locale: Locale, timeZone: string): DashboardFormat {
  const plain = new Intl.NumberFormat(locale, { maximumFractionDigits: 3 });
  const small = new Intl.NumberFormat(locale, { maximumSignificantDigits: 3 });
  const axis = new Intl.NumberFormat(locale, { maximumFractionDigits: 2, notation: "compact" });
  const date = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeZone: "UTC" });
  const minute = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" });
  const second = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "medium", timeZone: "UTC" });
  // `timeStyle` takes no fractional seconds: they go after the seconds of the same pattern.
  const decimal = new Intl.NumberFormat(locale).formatToParts(1.5).find((part) => part.type === "decimal")?.value;
  const withMillis = (ms: number) => {
    const millis = String(((ms % 1000) + 1000) % 1000).padStart(3, "0");
    return second
      .formatToParts(ms)
      .map((part) => (part.type === "second" ? `${part.value}${decimal ?? "."}${millis}` : part.value))
      .join("");
  };
  const clockFields = { hour: "2-digit", minute: "2-digit", hourCycle: "h23", timeZone: "UTC" } as const;
  const clock = new Intl.DateTimeFormat(locale, clockFields);
  const clockSecond = new Intl.DateTimeFormat(locale, { ...clockFields, second: "2-digit" });
  const clockMilli = new Intl.DateTimeFormat(locale, { ...clockFields, second: "2-digit", fractionalSecondDigits: 3 });
  const dayMonth = new Intl.DateTimeFormat(locale, { month: "numeric", day: "numeric", timeZone: "UTC" });
  const yearMonth = new Intl.DateTimeFormat(locale, { year: "numeric", month: "numeric", timeZone: "UTC" });
  const zoned = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "medium", timeZone });

  const number = (value: number | null) => {
    if (value === null || !Number.isFinite(value)) return NO_VALUE;
    return value !== 0 && Math.abs(value) < 1 ? small.format(value) : plain.format(value);
  };
  const wallTime = (ms: number, dateOnly = false) => {
    if (dateOnly) return date.format(ms);
    if (ms % 1000 !== 0) return withMillis(ms);
    return ms % 60_000 === 0 ? minute.format(ms) : second.format(ms);
  };
  return {
    locale,
    number,
    withUnit: (text, unit) => (unit === undefined || text === NO_VALUE ? text : `${text} ${unit}`),
    wallTime,
    cell: (type, cell) => {
      if (cell === null) return NO_VALUE;
      switch (type) {
        case "number":
          return typeof cell === "number" ? number(cell) : NO_VALUE;
        case "bool":
          return translate(locale, cell === true ? "dashboard.value.true" : "dashboard.value.false");
        case "time": {
          const ms = wallMs(cell);
          return ms === null ? String(cell) : wallTime(ms, isDateOnly(cell));
        }
        case "text":
          return String(cell);
      }
    },
    axisTime: (ms, spanMs) => {
      // Ticks sit on round times, so each label shows the finest unit its tick has: zoomed in to
      // seconds the labels say seconds, and only a tick at midnight names its day.
      if (ms % 1000 !== 0) return clockMilli.format(ms);
      if (ms % 60_000 !== 0) return clockSecond.format(ms);
      if (ms % DAY_MS !== 0) return clock.format(ms);
      if (spanMs > 400 * DAY_MS && new Date(ms).getUTCDate() === 1) return yearMonth.format(ms);
      return dayMonth.format(ms);
    },
    axisNumber: (value) => axis.format(value),
    instant: (text) => {
      const ms = Date.parse(text);
      return Number.isNaN(ms) ? text : zoned.format(ms);
    },
  };
}
