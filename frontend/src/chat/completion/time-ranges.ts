import { fromWallClock, wallClock } from "../../pages/device/time.ts";

// The time ranges an `@` offers (design 「時間範圍」): worked out here, in the user's time zone, so
// they are on the list the moment it opens and need no server. Weeks start on Monday.

export const RANGE_IDS = [
  "today",
  "yesterday",
  "this_week",
  "last_week",
  "this_month",
  "last_month",
  "last_7_days",
  "last_quarter",
] as const;
export type RangeId = (typeof RANGE_IDS)[number];

export interface TimeRange {
  id: RangeId;
  /** Inclusive start, as an instant (milliseconds). */
  from: number;
  /** Exclusive end; for a range that runs up to now, the moment it was worked out. */
  to: number;
  /** Whether the range runs up to now (today, this week, this month, the last 7 days). */
  open: boolean;
}

/** The ranges as they are at `now` in `timeZone` (an IANA name). */
export function timeRanges(now: number, timeZone: string): TimeRange[] {
  const wall = wallClock(now, timeZone);
  // `Date.UTC` carries an overflowing day or month into the next month or year, so offsets may cross both.
  const day = (offset: number) => fromWallClock(Date.UTC(wall.year, wall.month - 1, wall.day + offset), timeZone);
  const month = (offset: number) => fromWallClock(Date.UTC(wall.year, wall.month - 1 + offset, 1), timeZone);
  // 0 on Monday, so the week's first day is `day(-sinceMonday)`.
  const sinceMonday = (new Date(Date.UTC(wall.year, wall.month - 1, wall.day)).getUTCDay() + 6) % 7;
  const quarterFirstMonth = Math.floor((wall.month - 1) / 3) * 3;
  const quarter = (offset: number) => fromWallClock(Date.UTC(wall.year, quarterFirstMonth + offset, 1), timeZone);
  return [
    { id: "today", from: day(0), to: now, open: true },
    { id: "yesterday", from: day(-1), to: day(0), open: false },
    { id: "this_week", from: day(-sinceMonday), to: now, open: true },
    { id: "last_week", from: day(-sinceMonday - 7), to: day(-sinceMonday), open: false },
    { id: "this_month", from: month(0), to: now, open: true },
    { id: "last_month", from: month(-1), to: month(0), open: false },
    { id: "last_7_days", from: day(-6), to: now, open: true },
    { id: "last_quarter", from: quarter(-3), to: quarter(0), open: false },
  ];
}

/**
 * How a range's span is written in the list: `10/19 (Mon) – 10/25 (Sun)` for a week, `10/01 – 10/31`
 * for other closed ranges, and `11/01 – now` for one that runs up to now (`11/01 00:00 – now` for
 * today). `nowWord` is the word for now in the user's language.
 */
export function describeRange(range: TimeRange, locale: string, timeZone: string, nowWord: string): string {
  const monthDay = new Intl.DateTimeFormat(locale, { timeZone, month: "2-digit", day: "2-digit" });
  const weekday = new Intl.DateTimeFormat(locale, { timeZone, weekday: "short" });
  const week = range.id === "this_week" || range.id === "last_week";
  const date = (instant: number) => {
    const text = monthDay.format(new Date(instant));
    return week ? `${text} (${weekday.format(new Date(instant))})` : text;
  };
  if (range.open) return `${date(range.from)}${range.id === "today" ? " 00:00" : ""} – ${nowWord}`;
  const first = date(range.from);
  const last = date(range.to - 1);
  // A single day (yesterday) is just that day.
  return first === last ? first : `${first} – ${last}`;
}
