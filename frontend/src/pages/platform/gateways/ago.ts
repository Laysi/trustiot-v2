import type { Locale } from "../../../i18n/messages.ts";

const UNITS: readonly [Intl.RelativeTimeFormatUnit, number][] = [
  ["day", 86_400_000],
  ["hour", 3_600_000],
  ["minute", 60_000],
  ["second", 1000],
];

/** "12 seconds ago", "3 hours ago", in the user's language; the largest unit that fits. */
export function formatAgo(instant: string, now: number, locale: Locale): string {
  const elapsed = Math.max(0, now - Date.parse(instant));
  const formatter = new Intl.RelativeTimeFormat(locale, { numeric: "always" });
  for (const [unit, size] of UNITS) {
    if (elapsed >= size || unit === "second") return formatter.format(-Math.floor(elapsed / size), unit);
  }
  return "";
}
