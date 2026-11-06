import type { Locale } from "../../i18n/messages.ts";

/** 2026-11-02 is a Monday: the weekday `day` (1 Monday to 7 Sunday) is read off the matching date. */
function sampleDate(day: number): Date {
  return new Date(Date.UTC(2026, 10, 1 + day));
}

export function weekdayName(day: number, locale: Locale, style: "long" | "short" = "long"): string {
  return new Intl.DateTimeFormat(locale, { weekday: style, timeZone: "UTC" }).format(sampleDate(day));
}

export const WEEKDAYS = [1, 2, 3, 4, 5, 6, 7] as const;
