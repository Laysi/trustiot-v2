import { useId } from "react";

import type { Backtest } from "../../api/pending/alerts.ts";
import { useI18n } from "../../i18n/useI18n.ts";

const BAR = 16;
const STEP = 20;
const HEIGHT = 56;

/**
 * The backtest's daily trigger counts as bars (design 「回測結果」, decision Q8): SVG with the
 * position and size in attributes and the colour in classes, as the device page's chart is, so
 * no `style` attribute is needed (CSP) and no chart library is loaded for 14 bars. A day with no
 * trigger is a short grey tick, so the window's length shows. The picture is `role="img"` with a
 * sentence of what it shows; the table of the triggers beside it is the full alternative.
 */
export function BacktestChart({ backtest }: { backtest: Backtest }) {
  const { t } = useI18n();
  const id = useId();
  const days = backtest.daily_counts;
  const max = Math.max(1, ...days.map((day) => day.count));
  const width = Math.max(1, days.length) * STEP - (STEP - BAR);
  const withTriggers = days.filter((day) => day.count > 0);
  const summary =
    withTriggers.length === 0
      ? t("alerts.backtest.chart_none")
      : t("alerts.backtest.chart_summary", {
          days: withTriggers.map((day) => t("alerts.backtest.chart_day", { date: day.date.slice(5), count: day.count })).join(", "),
        });
  const first = days[0];
  const middle = days[Math.floor(days.length / 2)];
  const last = days[days.length - 1];
  return (
    <figure className="grid gap-1">
      <figcaption id={`${id}-caption`} className="text-sm text-muted-foreground">
        {t("alerts.backtest.chart_caption")}
      </figcaption>
      <svg
        role="img"
        aria-labelledby={`${id}-caption`}
        aria-describedby={`${id}-summary`}
        viewBox={`0 0 ${width} ${HEIGHT + 2}`}
        className="h-16 w-full max-w-md"
        preserveAspectRatio="none"
      >
        {days.map((day, index) => {
          const height = day.count === 0 ? 2 : Math.max(4, (day.count / max) * HEIGHT);
          return (
            <rect
              key={day.date}
              x={index * STEP}
              y={HEIGHT + 2 - height}
              width={BAR}
              height={height}
              rx={1}
              className={day.count === 0 ? "fill-muted-foreground/40" : "fill-primary"}
            />
          );
        })}
      </svg>
      {first && middle && last ? (
        <div aria-hidden="true" className="flex max-w-md justify-between text-xs text-muted-foreground">
          <span>{first.date.slice(5)}</span>
          <span>{middle.date.slice(5)}</span>
          <span>{last.date.slice(5)}</span>
        </div>
      ) : null}
      <p id={`${id}-summary`} className="sr-only">
        {summary}
      </p>
    </figure>
  );
}
