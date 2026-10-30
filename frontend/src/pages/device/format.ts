import { useMemo } from "react";

import type { Interval, Reading } from "../../api/devices.ts";
import type { MessageId } from "../../i18n/messages.ts";
import { type I18n, useI18n } from "../../i18n/useI18n.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { DURATION, wallClock } from "./time.ts";

export const INTERVAL_LABELS: Readonly<Record<Interval, MessageId>> = {
  raw: "device.interval.raw",
  "1m": "device.interval.1m",
  "5m": "device.interval.5m",
  "15m": "device.interval.15m",
  "1h": "device.interval.1h",
  "3h": "device.interval.3h",
  "6h": "device.interval.6h",
  "12h": "device.interval.12h",
  "1d": "device.interval.1d",
};

/** The device page's formatting, in the user's language and time zone. */
export interface DeviceFormat {
  t: I18n["t"];
  locale: I18n["locale"];
  timeZone: string;
  /** An instant to the second: readings can be a second apart. */
  time(instant: number | string): string;
  /** An instant to the minute: range bounds and bucket starts. */
  minute(instant: number | string): string;
  number(value: number): string;
  /** A stored reading as text: a bool is on or off, NaN or infinity says so. */
  reading(reading: Reading): string;
  /** A share (0 to 1) as a percentage. */
  share(value: number): string;
  /** A label on the time axis: the time of day, or the date at a local midnight or a step of days. */
  tick(instant: number, step: number): string;
  /** How long `ms` is, in its largest whole unit (seconds, minutes, hours or days). */
  duration(ms: number): string;
}

export function useDeviceFormat(): DeviceFormat {
  const { t, locale } = useI18n();
  const { timeZone } = usePreferences();
  return useMemo<DeviceFormat>(() => {
    const seconds = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "medium", timeZone });
    const minutes = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short", timeZone });
    const clock = new Intl.DateTimeFormat(locale, { hour: "2-digit", minute: "2-digit", hourCycle: "h23", timeZone });
    const date = new Intl.DateTimeFormat(locale, { month: "numeric", day: "numeric", timeZone });
    const numbers = new Intl.NumberFormat(locale, { maximumFractionDigits: 3 });
    const percent = new Intl.NumberFormat(locale, { style: "percent", maximumFractionDigits: 1 });
    const toDate = (instant: number | string) => (typeof instant === "string" ? new Date(instant) : new Date(instant));
    return {
      t,
      locale,
      timeZone,
      time: (instant) => seconds.format(toDate(instant)),
      minute: (instant) => minutes.format(toDate(instant)),
      number: (value) => numbers.format(value),
      share: (value) => percent.format(value),
      reading: (reading) => {
        switch (reading.type) {
          case "float":
            return reading.value === null ? t("device.value.not_a_number") : numbers.format(reading.value);
          case "int":
            return numbers.format(reading.value);
          case "bool":
            return reading.value ? t("device.value.on") : t("device.value.off");
          case "string":
            return reading.value;
        }
      },
      tick: (instant, step) => {
        if (step >= DURATION.day) return date.format(instant);
        const wall = wallClock(instant, timeZone);
        return wall.hour === 0 && wall.minute === 0 ? date.format(instant) : clock.format(instant);
      },
      duration: (ms) => {
        const units = [
          ["day", DURATION.day],
          ["hour", DURATION.hour],
          ["minute", DURATION.minute],
          ["second", 1000],
        ] as const;
        const [unit, size] = units.find(([, length]) => ms >= length && ms % length === 0) ?? ["second", 1000];
        return new Intl.NumberFormat(locale, { style: "unit", unit, unitDisplay: "long" }).format(ms / size);
      },
    };
  }, [t, locale, timeZone]);
}
