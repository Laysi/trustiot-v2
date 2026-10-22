import type { Locale } from "./messages.ts";

/**
 * An instant as the user reads it: in their locale and the time zone of their preferences
 * (an IANA name, already resolved from "auto"). Accepts the API's RFC 3339 strings.
 */
export function formatDateTime(instant: Date | string, locale: Locale, timeZone: string): string {
  const date = typeof instant === "string" ? new Date(instant) : instant;
  return new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short", timeZone }).format(date);
}

export function formatNumber(value: number, locale: Locale): string {
  return new Intl.NumberFormat(locale).format(value);
}
