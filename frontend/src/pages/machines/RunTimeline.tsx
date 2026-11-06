import { useId, useMemo } from "react";

import type { ProductionRun } from "../../api/pending/machines.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { fromWallClock, wallClock } from "../device/time.ts";

/** How many days the band shows, today included (design.md: 最近 7 天). */
export const TIMELINE_DAYS = 7;

const WIDTH = 560;
const ROW = 24;
/** One class per series: the same part always gets the same one, in order of first appearance. */
const SERIES = [
  "fill-chart-1/25 stroke-chart-1",
  "fill-chart-2/25 stroke-chart-2",
  "fill-chart-3/25 stroke-chart-3",
  "fill-chart-4/25 stroke-chart-4",
  "fill-chart-5/25 stroke-pink-600",
  "fill-chart-6/25 stroke-chart-6",
] as const;
const LABEL_MIN_WIDTH = 64;

const pad = (value: number) => String(value).padStart(2, "0");

/** The first instant of the local day `daysAgo` days before the one of `now`. */
export function dayStart(now: number, daysAgo: number, timeZone: string): number {
  const wall = wallClock(now, timeZone);
  return fromWallClock(Date.UTC(wall.year, wall.month - 1, wall.day - daysAgo), timeZone);
}

/**
 * The records of the last days, one row per day: a band for each, with the part number on it, and
 * the gaps hatched (the design's "沒有紀錄，可補登"). It is a picture of what the table below it
 * says, so it is `aria-hidden`. Drawn as SVG attributes (no `style` attribute, CSP).
 */
export function RunTimeline({ runs, now }: { runs: readonly ProductionRun[]; now: number }) {
  const { t, locale } = useI18n();
  const { timeZone } = usePreferences();
  const hatchId = `hatch-${useId().replaceAll(":", "")}`;

  const view = useMemo(() => {
    const partOrder: string[] = [];
    for (const run of [...runs].sort((a, b) => Date.parse(a.started_at) - Date.parse(b.started_at))) {
      if (!partOrder.includes(run.part_number)) partOrder.push(run.part_number);
    }
    const weekday = new Intl.DateTimeFormat(locale, { weekday: "short", timeZone });
    const days = Array.from({ length: TIMELINE_DAYS }, (_, daysAgo) => {
      const from = dayStart(now, daysAgo, timeZone);
      const to = dayStart(now, daysAgo - 1, timeZone);
      const wall = wallClock(from, timeZone);
      const bands = runs.flatMap((run) => {
        const start = Math.max(Date.parse(run.started_at), from);
        const end = Math.min(run.ended_at === null ? now : Date.parse(run.ended_at), to);
        if (end <= start) return [];
        const x = ((start - from) / (to - from)) * WIDTH;
        const width = Math.max(((end - start) / (to - from)) * WIDTH, 2);
        return [{ run, x, width, series: SERIES[partOrder.indexOf(run.part_number) % SERIES.length] }];
      });
      return { label: `${pad(wall.month)}-${pad(wall.day)} (${weekday.format(from)})`, from, bands };
    });
    return { days };
  }, [runs, now, locale, timeZone]);

  return (
    <div aria-hidden="true" className="grid gap-1 text-xs">
      {view.days.map((day) => (
        <div key={day.from} className="grid grid-cols-[6.5rem_1fr] items-center gap-2">
          <span className="text-muted-foreground">{day.label}</span>
          <svg viewBox={`0 0 ${WIDTH} ${ROW}`} className="h-6 w-full" focusable="false">
            <title>{day.label}</title>
            <defs>
              <pattern id={hatchId} width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">
                <line x1="0" y1="0" x2="0" y2="6" className="stroke-border" strokeWidth="2" />
              </pattern>
            </defs>
            <rect x="0" y="2" width={WIDTH} height={ROW - 4} rx="3" fill={`url(#${hatchId})`} />
            {day.bands.map(({ run, x, width, series }) => (
              <g key={`${run.run_id}`}>
                <rect x={x} y="2" width={width} height={ROW - 4} rx="3" className={series} strokeWidth="1" />
                {width >= LABEL_MIN_WIDTH ? (
                  <text x={x + 6} y={ROW / 2 + 4} className="fill-foreground font-mono text-[11px]">
                    {run.part_number}
                  </text>
                ) : null}
              </g>
            ))}
          </svg>
        </div>
      ))}
      <div className="grid grid-cols-[6.5rem_1fr] gap-2">
        <span />
        <div className="flex justify-between text-muted-foreground">
          {[0, 6, 12, 18, 24].map((hour) => (
            <span key={hour}>{`${pad(hour)}:00`}</span>
          ))}
        </div>
      </div>
      <p className="flex flex-wrap gap-x-4 gap-y-1 pt-1 text-muted-foreground">
        <span>{t("machines.timeline.gap")}</span>
        <span>{t("machines.timeline.bands")}</span>
      </p>
    </div>
  );
}
