import type { ReportKind } from "../../api/pending/reports.ts";

// Calendar arithmetic for the schedule form's preview sentence ("the first report goes out on
// 11-02 (Mon) 08:00 and covers 10-26 – 11-01"). Dates are `YYYY-MM-DD` strings in the schedule's
// own time zone; only whole days are added, so a daylight saving change never moves a date.

const DAY_MS = 86_400_000;

function toUtc(date: string): number {
  const [year, month, day] = date.split("-").map(Number);
  return Date.UTC(year ?? 1970, (month ?? 1) - 1, day ?? 1);
}

function fromUtc(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

export function addDays(date: string, days: number): string {
  return fromUtc(toUtc(date) + days * DAY_MS);
}

/** 1 for Monday to 7 for Sunday. */
export function weekdayOf(date: string): number {
  const day = new Date(toUtc(date)).getUTCDay();
  return day === 0 ? 7 : day;
}

/** The local calendar date and the minutes since midnight of `instant` in `timeZone`. */
export function zonedNow(instant: Date, timeZone: string): { date: string; minutes: number } {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  }).formatToParts(instant);
  const part = (type: string) => parts.find((item) => item.type === type)?.value ?? "0";
  return {
    date: `${part("year")}-${part("month")}-${part("day")}`,
    minutes: Number(part("hour")) * 60 + Number(part("minute")),
  };
}

/** `HH:MM` as minutes since midnight, or `null` when it is not a time of day. */
export function parseTime(time: string): number | null {
  const match = /^([01]\d|2[0-3]):([0-5]\d)$/.exec(time);
  return match ? Number(match[1]) * 60 + Number(match[2]) : null;
}

export interface Occurrence {
  /** The local date the report goes out. */
  sendDate: string;
  /** The first and last day (inclusive) of what it covers. */
  periodStart: string;
  periodEnd: string;
}

/**
 * The first send of a schedule after `now`: a weekly report (`sendOn` is the weekday) covers the
 * calendar week before the send date, Monday to Sunday; a monthly one (`sendOn` is the day of the
 * month, at most 28) covers the calendar month before it. `null` when `time` is not a time.
 */
export function nextOccurrence(
  now: Date,
  schedule: { kind: ReportKind; sendOn: number; time: string; timeZone: string },
): Occurrence | null {
  const at = parseTime(schedule.time);
  if (at === null) return null;
  const today = zonedNow(now, schedule.timeZone);
  for (let offset = 0; offset <= 62; offset += 1) {
    const date = addDays(today.date, offset);
    const matches =
      schedule.kind === "weekly_energy"
        ? weekdayOf(date) === schedule.sendOn
        : Number(date.slice(8, 10)) === schedule.sendOn;
    if (!matches || (offset === 0 && at <= today.minutes)) continue;
    if (schedule.kind === "weekly_energy") {
      const monday = addDays(date, 1 - weekdayOf(date));
      return { sendDate: date, periodStart: addDays(monday, -7), periodEnd: addDays(monday, -1) };
    }
    const year = Number(date.slice(0, 4));
    const month = Number(date.slice(5, 7));
    return {
      sendDate: date,
      periodStart: fromUtc(Date.UTC(year, month - 2, 1)),
      periodEnd: fromUtc(Date.UTC(year, month - 1, 0)),
    };
  }
  return null;
}

/** `MM-DD` of a `YYYY-MM-DD` date. */
export function monthDay(date: string): string {
  return date.slice(5);
}
