import { useMemo } from "react";

import type { ChangeEntry } from "../../api/pending/machines.ts";
import { type I18n, useI18n } from "../../i18n/useI18n.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { DURATION, wallClock } from "../device/time.ts";

const pad = (value: number) => String(value).padStart(2, "0");

/** The machine pages' formatting, in the user's language and time zone (design.md, 「時間」). */
export interface MachineFormat {
  t: I18n["t"];
  timeZone: string;
  /**
   * A moment as the design writes it: `14:05` today, "Yesterday 22:05", `10-31 16:22` this year,
   * the year too before that. `now` is the page's clock.
   */
  when(instant: string, now: number): string;
  /** How long a record has been running, in hours and minutes ("6 h 20 min"), days past one day. */
  elapsed(ms: number): string;
  number(value: number): string;
  /** A correction's value in the change record: a time as `10-31 14:10`, empty as a dash. */
  changeValue(field: ChangeEntry["changes"][number]["field"], value: string | null, now: number): string;
}

function ymd(instant: number, timeZone: string): number {
  const wall = wallClock(instant, timeZone);
  return Date.UTC(wall.year, wall.month - 1, wall.day);
}

export function useMachineFormat(): MachineFormat {
  const { t, locale } = useI18n();
  const { timeZone } = usePreferences();
  return useMemo<MachineFormat>(() => {
    const numbers = new Intl.NumberFormat(locale);
    const when = (instant: string, now: number) => {
      const at = Date.parse(instant);
      const wall = wallClock(at, timeZone);
      const clock = `${pad(wall.hour)}:${pad(wall.minute)}`;
      const days = Math.round((ymd(now, timeZone) - ymd(at, timeZone)) / DURATION.day);
      if (days === 0) return clock;
      if (days === 1) return t("machines.when.yesterday", { time: clock });
      const date = `${pad(wall.month)}-${pad(wall.day)}`;
      return wallClock(now, timeZone).year === wall.year ? `${date} ${clock}` : `${wall.year}-${date} ${clock}`;
    };
    return {
      t,
      timeZone,
      when,
      number: (value) => numbers.format(value),
      elapsed: (ms) => {
        const minutes = Math.floor(ms / DURATION.minute);
        if (minutes < 1) return t("machines.elapsed.just_started");
        const days = Math.floor(minutes / 1440);
        const hours = Math.floor((minutes % 1440) / 60);
        const rest = minutes % 60;
        if (days > 0) return t("machines.elapsed.days_hours", { days, hours });
        if (hours > 0)
          return rest === 0
            ? t("machines.elapsed.hours", { hours })
            : t("machines.elapsed.hours_minutes", { hours, minutes: rest });
        return t("machines.elapsed.minutes", { minutes: rest });
      },
      changeValue: (field, value, now) => {
        if (value === null) return t("machines.change.empty");
        return field === "started_at" || field === "ended_at" ? when(value, now) : value;
      },
    };
  }, [t, locale, timeZone]);
}
