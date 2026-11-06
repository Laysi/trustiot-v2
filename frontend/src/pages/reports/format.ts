import type { Locale } from "../../i18n/messages.ts";

/** kWh and kg, with at most one decimal. */
export function formatAmount(value: number, locale: Locale): string {
  return new Intl.NumberFormat(locale, { maximumFractionDigits: 1 }).format(value);
}

/** A change given as a ratio (0.042) as a signed percent (+4.2%); `—` when there is nothing to compare with. */
export function formatChange(ratio: number | null, locale: Locale): string {
  if (ratio === null) return "—";
  return new Intl.NumberFormat(locale, {
    style: "percent",
    maximumFractionDigits: 1,
    signDisplay: "exceptZero",
  }).format(ratio);
}

/** A `YYYY-MM-DD` date as its month and year ("October 2026"). */
export function formatMonth(date: string, locale: Locale): string {
  const [year, month] = date.split("-").map(Number);
  return new Intl.DateTimeFormat(locale, { year: "numeric", month: "long", timeZone: "UTC" }).format(
    new Date(Date.UTC(year ?? 1970, (month ?? 1) - 1, 1)),
  );
}
