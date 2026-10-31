// Times in dataset results (frontend/sandbox/README.md, 「時間」). A page draws with wall-clock
// times: the date and time of day as the query wrote them, held in a `Date` whose UTC fields are
// that wall time, so `d3.scaleUtc` and `utcFormat` show the query's local time whatever the
// viewer's own zone is. The runtime and the comparison both use this module, so a page and the
// check read every time the same way.

const DATE = /^(\d{4})-(\d{2})-(\d{2})$/;
const SQL_TIME = /^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?$/;
const RFC3339 = /^(\d{4})-(\d{2})-(\d{2})[Tt ](\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,9}))?([Zz]|([+-])(\d{2}):(\d{2}))$/;

/** A `Date` with these UTC fields, or `null` when they name no real date or time. */
function utcDate(
  year: number,
  month: number,
  day: number,
  hour: number,
  minute: number,
  second: number,
  millisecond: number,
): Date | null {
  if (month < 1 || month > 12 || day < 1 || hour > 23 || minute > 59 || second > 59) return null;
  const date = new Date(0);
  // setUTCFullYear, unlike Date.UTC, keeps the years 0 to 99 as they are.
  date.setUTCFullYear(year, month - 1, day);
  date.setUTCHours(hour, minute, second, millisecond);
  if (date.getUTCDate() !== day || date.getUTCMonth() !== month - 1) return null;
  return date;
}

/** Milliseconds from a fraction of a second's digits: the first three, the rest dropped. */
function milliseconds(fraction: string | undefined): number {
  return fraction === undefined ? 0 : Number(`${fraction}00`.slice(0, 3));
}

/**
 * A result's time cell as a wall-clock `Date`: `YYYY-MM-DD`, `YYYY-MM-DD hh:mm:ss[.f…]` (SQL, in
 * the zone the query named) or RFC 3339 with an offset (semantic sources, in the request's
 * zone); the offset is ignored. Digits past milliseconds are dropped. `null` for anything else.
 */
export function parseWallTime(text: string): Date | null {
  let match = DATE.exec(text);
  if (match !== null) return utcDate(Number(match[1]), Number(match[2]), Number(match[3]), 0, 0, 0, 0);
  match = SQL_TIME.exec(text) ?? RFC3339.exec(text);
  if (match === null) return null;
  return utcDate(
    Number(match[1]),
    Number(match[2]),
    Number(match[3]),
    Number(match[4]),
    Number(match[5]),
    Number(match[6]),
    milliseconds(match[7]),
  );
}

/** The instant an RFC 3339 time with an offset names, in milliseconds since the epoch. */
export function parseInstant(text: string): number | null {
  const match = RFC3339.exec(text);
  if (match === null) return null;
  const wall = utcDate(
    Number(match[1]),
    Number(match[2]),
    Number(match[3]),
    Number(match[4]),
    Number(match[5]),
    Number(match[6]),
    milliseconds(match[7]),
  );
  if (wall === null) return null;
  const offsetHours = Number(match[10] ?? 0);
  const offsetMinutes = Number(match[11] ?? 0);
  if (offsetHours > 23 || offsetMinutes > 59) return null;
  const sign = match[9] === "-" ? -1 : 1;
  return wall.getTime() - sign * (offsetHours * 60 + offsetMinutes) * 60_000;
}

const zoneFormatters: Record<string, Intl.DateTimeFormat> = Object.create(null);

function zoneFormatter(tz: string): Intl.DateTimeFormat {
  const cached = zoneFormatters[tz];
  if (cached !== undefined) return cached;
  let formatter: Intl.DateTimeFormat;
  try {
    formatter = new Intl.DateTimeFormat("en-US", {
      timeZone: tz,
      hourCycle: "h23",
      year: "numeric",
      month: "numeric",
      day: "numeric",
      hour: "numeric",
      minute: "numeric",
      second: "numeric",
    });
  } catch {
    // A zone the browser does not know: show UTC rather than fail the page.
    formatter = new Intl.DateTimeFormat("en-US", {
      timeZone: "UTC",
      hourCycle: "h23",
      year: "numeric",
      month: "numeric",
      day: "numeric",
      hour: "numeric",
      minute: "numeric",
      second: "numeric",
    });
  }
  zoneFormatters[tz] = formatter;
  return formatter;
}

/**
 * An instant (RFC 3339 with an offset) as the wall-clock `Date` of zone `tz` (an IANA name; UTC
 * when the browser does not know it): how `m|…|queried_at`, `range_from` and `range_to` read.
 */
export function instantToWall(text: string, tz: string): Date | null {
  const instant = parseInstant(text);
  if (instant === null) return null;
  const fields: Record<string, number> = Object.create(null);
  for (const part of zoneFormatter(tz).formatToParts(new Date(instant))) {
    if (part.type !== "literal") fields[part.type] = Number(part.value);
  }
  const millisecond = ((instant % 1000) + 1000) % 1000;
  return utcDate(
    fields.year ?? Number.NaN,
    fields.month ?? Number.NaN,
    fields.day ?? Number.NaN,
    fields.hour ?? Number.NaN,
    fields.minute ?? Number.NaN,
    fields.second ?? Number.NaN,
    millisecond,
  );
}

/**
 * A wall-clock `Date` as a tick reference writes it (`YYYY-MM-DDThh:mm:ss.sss`), or `null` for a
 * date outside the years 0000 to 9999 or an invalid one.
 */
export function tickTimeText(date: Date): string | null {
  const time = date.getTime();
  if (!Number.isFinite(time)) return null;
  const year = date.getUTCFullYear();
  if (year < 0 || year > 9999) return null;
  const iso = date.toISOString();
  return iso.endsWith("Z") ? iso.slice(0, -1) : null;
}

/** The `Date` a tick reference's time text names. */
export function tickTimeDate(text: string): Date {
  return new Date(`${text}Z`);
}
