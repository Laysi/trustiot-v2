import { useMemo } from "react";

import type { Interval, Reading } from "../../api/devices.ts";
import type { MessageId } from "../../i18n/messages.ts";
import { type I18n, useI18n } from "../../i18n/useI18n.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { DURATION } from "./time.ts";

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
  /** The unit of the data point these values are in (its model's `unit`), or null: `useDeviceFormat`'s argument. */
  unit: string | null;
  /** A number in the data point's unit, `21.5 °C`; the number alone without a unit. */
  measure(value: number): string;
  /** A float for the readings list: two decimals (three significant digits below 0.01, so a small value is not 0.00). */
  fixed(value: number): string;
  /** A stored reading as text: a bool is on or off, NaN or infinity says so. */
  reading(reading: Reading): string;
  /** A share (0 to 1) as a percentage. */
  share(value: number): string;
  /** A day, without the time: when a device was added. */
  date(instant: number | string): string;
  /** How long `ms` is, in its largest whole unit (seconds, minutes, hours or days). */
  duration(ms: number): string;
  /** How long ago `ms` was, in its largest unit: 12 seconds ago, 3 hours ago. */
  ago(ms: number): string;
  /** How long `ms` is in up to two units, for a stretch of time: 3 hours 14 minutes. */
  elapsed(ms: number): string;
}

/** The device page's formatting; `unit` is the unit of the data point whose values it writes with `measure`. */
export function useDeviceFormat(unit: string | null = null): DeviceFormat {
  const { t, locale } = useI18n();
  const { timeZone } = usePreferences();
  return useMemo<DeviceFormat>(() => {
    const seconds = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "medium", timeZone });
    const minutes = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short", timeZone });
    const day = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeZone });
    const relative = new Intl.RelativeTimeFormat(locale, { numeric: "always", style: "long" });
    const numbers = new Intl.NumberFormat(locale, { maximumFractionDigits: 3 });
    const twoDecimals = new Intl.NumberFormat(locale, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    const significant = new Intl.NumberFormat(locale, { maximumSignificantDigits: 3 });
    const percent = new Intl.NumberFormat(locale, { style: "percent", maximumFractionDigits: 1 });
    const toDate = (instant: number | string) => (typeof instant === "string" ? new Date(instant) : new Date(instant));
    return {
      t,
      locale,
      timeZone,
      time: (instant) => seconds.format(toDate(instant)),
      minute: (instant) => minutes.format(toDate(instant)),
      number: (value) => numbers.format(value),
      unit,
      measure: (value) => (unit === null ? numbers.format(value) : `${numbers.format(value)} ${unit}`),
      fixed: (value) => (value !== 0 && Math.abs(value) < 0.01 ? significant.format(value) : twoDecimals.format(value)),
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
      date: (instant) => day.format(toDate(instant)),
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
      ago: (ms) => {
        const units = [
          ["day", DURATION.day],
          ["hour", DURATION.hour],
          ["minute", DURATION.minute],
        ] as const;
        const [unit, size] = units.find(([, length]) => ms >= length) ?? ["second", 1000];
        return relative.format(-Math.max(0, Math.floor(ms / size)), unit);
      },
      elapsed: (ms) => {
        const units = [
          ["day", DURATION.day],
          ["hour", DURATION.hour],
          ["minute", DURATION.minute],
          ["second", 1000],
        ] as const;
        const parts: string[] = [];
        let rest = Math.max(0, ms);
        for (const [unit, size] of units) {
          const whole = Math.floor(rest / size);
          rest -= whole * size;
          if (whole > 0 && parts.length < 2) {
            parts.push(new Intl.NumberFormat(locale, { style: "unit", unit, unitDisplay: "long" }).format(whole));
          }
        }
        return parts.length > 0
          ? parts.join(" ")
          : new Intl.NumberFormat(locale, { style: "unit", unit: "second", unitDisplay: "long" }).format(0);
      },
    };
  }, [t, locale, timeZone, unit]);
}
