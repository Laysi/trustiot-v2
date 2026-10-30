// Time arithmetic for the device page: the history's range in the user's time zone (an IANA name,
// "auto" already resolved), with no library: `Intl` knows every zone's offsets.

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

export const DURATION = { minute: MINUTE, hour: HOUR, day: DAY } as const;

const partsFormatters = new Map<string, Intl.DateTimeFormat>();

function partsFormatter(timeZone: string): Intl.DateTimeFormat {
  let formatter = partsFormatters.get(timeZone);
  if (formatter === undefined) {
    formatter = new Intl.DateTimeFormat("en-US", {
      timeZone,
      hourCycle: "h23",
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
    partsFormatters.set(timeZone, formatter);
  }
  return formatter;
}

/** The wall-clock fields of `instant` in `timeZone`. */
export function wallClock(instant: number, timeZone: string) {
  const fields: Record<string, number> = {};
  for (const part of partsFormatter(timeZone).formatToParts(new Date(instant))) {
    if (part.type !== "literal") fields[part.type] = Number(part.value);
  }
  return {
    year: fields.year ?? 1970,
    month: fields.month ?? 1,
    day: fields.day ?? 1,
    hour: fields.hour ?? 0,
    minute: fields.minute ?? 0,
    second: fields.second ?? 0,
  };
}

/** How far `timeZone`'s wall clock is ahead of UTC at `instant`, in milliseconds. */
export function offsetAt(instant: number, timeZone: string): number {
  const wall = wallClock(instant, timeZone);
  const asUtc = Date.UTC(wall.year, wall.month - 1, wall.day, wall.hour, wall.minute, wall.second);
  // Seconds of the instant itself are not in the fields' precision: drop them on both sides.
  return asUtc - Math.floor(instant / 1000) * 1000;
}

/**
 * The instant at which `timeZone`'s wall clock shows `local` (milliseconds of a UTC date that
 * holds the wall-clock fields). A time a daylight-saving jump skips comes out an hour later, a
 * time it repeats as its first occurrence.
 */
export function fromWallClock(local: number, timeZone: string): number {
  // The offset near the answer, then the offset at the instant it gives.
  const offset = offsetAt(local - offsetAt(local, timeZone), timeZone);
  const instant = local - offset;
  const actual = offsetAt(instant, timeZone);
  // The same offset: the wall time exists there. Otherwise a jump skipped it, and the offset
  // before the jump moves it past the gap.
  return actual === offset ? instant : local - actual;
}

const pad = (value: number, width = 2) => String(value).padStart(width, "0");

/** `instant` as the value of an `<input type="datetime-local">` in `timeZone`: `YYYY-MM-DDTHH:mm`. */
export function toLocalInput(instant: number, timeZone: string): string {
  const wall = wallClock(instant, timeZone);
  return `${pad(wall.year, 4)}-${pad(wall.month)}-${pad(wall.day)}T${pad(wall.hour)}:${pad(wall.minute)}`;
}

const LOCAL_INPUT = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2}))?$/;

/** The instant an `<input type="datetime-local">` value means in `timeZone`, or null when it is not one. */
export function fromLocalInput(value: string, timeZone: string): number | null {
  const match = LOCAL_INPUT.exec(value);
  if (!match) return null;
  const [year, month, day, hour, minute, second] = match.slice(1).map((field) => Number(field ?? 0)) as [
    number,
    number,
    number,
    number,
    number,
    number,
  ];
  const local = Date.UTC(year, month - 1, day, hour, minute, second);
  const check = new Date(local);
  // Date.UTC rolls 2026-02-31 over to March: such a value is not a date.
  if (check.getUTCMonth() !== month - 1 || check.getUTCDate() !== day || hour > 23 || minute > 59) return null;
  return fromWallClock(local, timeZone);
}

/** The ranges the history offers, ending now. */
export const PRESETS = [
  { id: "1h", duration: HOUR },
  { id: "6h", duration: 6 * HOUR },
  { id: "24h", duration: DAY },
  { id: "7d", duration: 7 * DAY },
  { id: "30d", duration: 30 * DAY },
  { id: "90d", duration: 90 * DAY },
] as const;

export type PresetId = (typeof PRESETS)[number]["id"];
export const DEFAULT_PRESET: PresetId = "24h";

export function isPresetId(value: string | null): value is PresetId {
  return PRESETS.some((preset) => preset.id === value);
}

export function presetDuration(id: PresetId): number {
  return PRESETS.find((preset) => preset.id === id)?.duration ?? DAY;
}

/** The history's range as the address bar holds it: a preset ending now, or fixed instants. */
export type RangeChoice = { kind: "preset"; preset: PresetId } | { kind: "custom"; from: number; to: number };

/**
 * The range in the address bar: `range=<preset>`, or `from` and `to` as RFC 3339 instants with
 * `from` before `to`. Anything else is the default preset, so a mangled link still opens.
 */
export function parseRange(params: URLSearchParams): RangeChoice {
  const from = params.get("from");
  const to = params.get("to");
  if (from !== null && to !== null) {
    const start = Date.parse(from);
    const end = Date.parse(to);
    if (Number.isFinite(start) && Number.isFinite(end) && start < end) {
      return { kind: "custom", from: start, to: end };
    }
  }
  const preset = params.get("range");
  return { kind: "preset", preset: isPresetId(preset) ? preset : DEFAULT_PRESET };
}

/** Writes `choice` into `params` (a copy), replacing whatever range was there. */
export function withRange(params: URLSearchParams, choice: RangeChoice): URLSearchParams {
  const next = new URLSearchParams(params);
  next.delete("range");
  next.delete("from");
  next.delete("to");
  if (choice.kind === "preset") {
    if (choice.preset !== DEFAULT_PRESET) next.set("range", choice.preset);
  } else {
    next.set("from", new Date(choice.from).toISOString());
    next.set("to", new Date(choice.to).toISOString());
  }
  return next;
}

/** The instants a choice covers, a preset ending at `now`. */
export function resolveRange(choice: RangeChoice, now: number): { from: number; to: number } {
  if (choice.kind === "custom") return { from: choice.from, to: choice.to };
  return { from: now - presetDuration(choice.preset), to: now };
}

/** The bucket length of each interval the series API answers with; `raw` has none. */
export const INTERVAL_MS: Readonly<Record<string, number>> = {
  "1m": MINUTE,
  "5m": 5 * MINUTE,
  "15m": 15 * MINUTE,
  "1h": HOUR,
  "3h": 3 * HOUR,
  "6h": 6 * HOUR,
  "12h": 12 * HOUR,
  "1d": DAY,
};
