import { useMemo } from "react";

import type { Locale } from "../../i18n/messages.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";

const DAY = 86_400_000;
const MINUTE = 60_000;
const HOUR = 3_600_000;

interface Wall {
  year: number;
  month: number;
  day: number;
  hour: number;
  minute: number;
}

function wallClock(instant: number, timeZone: string): Wall {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone,
    year: "numeric",
    month: "numeric",
    day: "numeric",
    hour: "numeric",
    minute: "numeric",
    hourCycle: "h23",
  }).formatToParts(new Date(instant));
  const read = (type: string) => Number(parts.find((part) => part.type === type)?.value ?? 0);
  return { year: read("year"), month: read("month"), day: read("day"), hour: read("hour"), minute: read("minute") };
}

const two = (value: number) => String(value).padStart(2, "0");
const dayNumber = (wall: Wall) => Math.floor(Date.UTC(wall.year, wall.month - 1, wall.day) / DAY);

/**
 * An instant as docs/design/ms4-ms5/design.md 「時間」 writes it, in the user's time zone: today
 * `14:05`, yesterday with the word, this year `10-31 16:22`, an earlier year with the year; within
 * the last 24 hours the time is followed by how long ago it was. A time that has not come yet
 * (a limit's end) has no "ago".
 */
export function formatWhen(instant: number | string, now: number, locale: Locale, timeZone: string): string {
  const at = typeof instant === "string" ? Date.parse(instant) : instant;
  if (Number.isNaN(at)) return "—";
  const wall = wallClock(at, timeZone);
  const today = wallClock(now, timeZone);
  const clock = `${two(wall.hour)}:${two(wall.minute)}`;
  const apart = dayNumber(today) - dayNumber(wall);
  let text: string;
  if (apart === 0) {
    text = clock;
  } else if (apart === 1) {
    text = `${new Intl.RelativeTimeFormat(locale, { numeric: "auto" }).format(-1, "day")} ${clock}`;
  } else if (wall.year === today.year) {
    text = `${two(wall.month)}-${two(wall.day)} ${clock}`;
  } else {
    text = `${wall.year}-${two(wall.month)}-${two(wall.day)} ${clock}`;
  }
  const ago = now - at;
  if (ago < 0 || ago >= DAY) return text;
  const relative = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  const phrase = ago < HOUR ? relative.format(-Math.floor(ago / MINUTE), "minute") : relative.format(-Math.floor(ago / HOUR), "hour");
  return `${text} (${phrase})`;
}

/** A date with its year, to the minute, for the version history and the pause record. */
export function formatFull(instant: number | string, timeZone: string): string {
  const at = typeof instant === "string" ? Date.parse(instant) : instant;
  if (Number.isNaN(at)) return "—";
  const wall = wallClock(at, timeZone);
  return `${wall.year}-${two(wall.month)}-${two(wall.day)} ${two(wall.hour)}:${two(wall.minute)}`;
}

/** The month and day of an instant (`10-29`), for the backtest window and chart. */
export function formatDay(instant: number | string, timeZone: string): string {
  return formatFull(instant, timeZone).slice(5, 10);
}

export interface AlertsTime {
  when(instant: number | string): string;
  full(instant: number | string): string;
  day(instant: number | string): string;
}

/** The time formats in the user's language and time zone. */
export function useAlertsTime(): AlertsTime {
  const { locale, timeZone } = usePreferences();
  return useMemo<AlertsTime>(
    () => ({
      when: (instant) => formatWhen(instant, Date.now(), locale, timeZone),
      full: (instant) => formatFull(instant, timeZone),
      day: (instant) => formatDay(instant, timeZone),
    }),
    [locale, timeZone],
  );
}
