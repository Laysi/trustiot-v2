import { useMemo } from "react";

import type { I18n } from "../../i18n/useI18n.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";

/** The gateway page's dates and durations, in the user's language and time zone. */
export interface GatewayFormat {
  t: I18n["t"];
  number(value: number): string;
  /** The time of day, to the minute. */
  clock(instant: string | number): string;
  /** Month, day and time, to the minute (a state's start, a binding). */
  monthDayTime(instant: string | number): string;
  /** The date only (a certificate's expiry). */
  date(instant: string | number): string;
  /** The instant to the second, for a `title`. */
  exact(instant: string | number): string;
  /** How long before `now` the instant is (`12 seconds ago`), in the largest whole unit. */
  relative(instant: string | number, now: number): string;
  /** A length of time in seconds as days and hours, or hours and minutes. */
  uptime(seconds: number): string;
}

export function useGatewayFormat(): GatewayFormat {
  const { t, locale, formatNumber } = useI18n();
  const { timeZone } = usePreferences();
  return useMemo<GatewayFormat>(() => {
    const date = (instant: string | number) => new Date(instant);
    const clock = new Intl.DateTimeFormat(locale, { hour: "2-digit", minute: "2-digit", hourCycle: "h23", timeZone });
    const monthDayTime = new Intl.DateTimeFormat(locale, {
      month: "numeric",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
      hourCycle: "h23",
      timeZone,
    });
    const day = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeZone });
    const exact = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "medium", timeZone });
    const relative = new Intl.RelativeTimeFormat(locale, { numeric: "always" });
    return {
      t,
      number: formatNumber,
      clock: (instant) => clock.format(date(instant)),
      monthDayTime: (instant) => monthDayTime.format(date(instant)),
      date: (instant) => day.format(date(instant)),
      exact: (instant) => exact.format(date(instant)),
      relative: (instant, now) => {
        const seconds = Math.max(0, Math.round((now - new Date(instant).getTime()) / 1000));
        if (seconds < 60) return relative.format(-seconds, "second");
        if (seconds < 3600) return relative.format(-Math.floor(seconds / 60), "minute");
        if (seconds < 86_400) return relative.format(-Math.floor(seconds / 3600), "hour");
        return relative.format(-Math.floor(seconds / 86_400), "day");
      },
      uptime: (seconds) => {
        const days = Math.floor(seconds / 86_400);
        const hours = Math.floor((seconds % 86_400) / 3600);
        if (days > 0) return t("gateways.metric.uptime_days", { days: formatNumber(days), hours });
        return t("gateways.metric.uptime_hours", { hours, minutes: Math.floor((seconds % 3600) / 60) });
      },
    };
  }, [t, locale, timeZone, formatNumber]);
}
