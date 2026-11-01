import { focusRing } from "../components/ui/button.tsx";
import { cn } from "../lib/utils.ts";
import { useDashboardEnv } from "./context.tsx";

/** The series colors as classes, one per slot; written out so Tailwind finds them in the source. */
const SWATCHES = ["bg-chart-1", "bg-chart-2", "bg-chart-3", "bg-chart-4", "bg-chart-5", "bg-chart-6"] as const;

/** A series' color beside its name: identity is never color alone, the name is always there. */
export function Swatch(props: { slot: number; className?: string }) {
  return (
    <span
      aria-hidden="true"
      className={cn(
        "inline-block size-2.5 shrink-0 rounded-sm",
        SWATCHES[props.slot] ?? "bg-foreground",
        props.className,
      )}
    />
  );
}

export interface LegendEntry {
  key: string;
  label: string;
  slot: number;
}

/**
 * The series of a chart as toggle buttons: click or Space hides and shows a series. The canvas
 * legend is off, so the legend is reachable by keyboard and read as text.
 */
export function Legend(props: {
  entries: readonly LegendEntry[];
  hidden: ReadonlySet<string>;
  onToggle(key: string): void;
}) {
  const { t } = useDashboardEnv();
  if (props.entries.length < 2) return null;
  return (
    <ul aria-label={t("dashboard.legend.label")} className="flex flex-wrap gap-1.5">
      {props.entries.map((entry) => {
        const visible = !props.hidden.has(entry.key);
        return (
          <li key={entry.key}>
            <button
              type="button"
              aria-pressed={visible}
              onClick={() => props.onToggle(entry.key)}
              className={cn(
                "inline-flex max-w-56 items-center gap-1.5 rounded-md border px-2 py-1 text-xs transition-colors hover:bg-accent",
                visible ? "text-foreground" : "text-muted-foreground line-through decoration-1",
                focusRing,
              )}
            >
              <Swatch slot={entry.slot} className={visible ? undefined : "opacity-30"} />
              <span className="truncate">{entry.label}</span>
            </button>
          </li>
        );
      })}
    </ul>
  );
}

/** One line of a tooltip: the series' color and name, then its value. */
export function TipRow(props: { slot?: number; label: string; value: string; note?: string }) {
  return (
    <div className="flex items-baseline gap-2">
      {props.slot !== undefined ? <Swatch slot={props.slot} className="translate-y-px" /> : null}
      <span className="min-w-0 truncate text-muted-foreground">{props.label}</span>
      <span className="ml-auto pl-3 font-medium tabular-nums" data-testid="tip-value">
        {props.value}
      </span>
      {props.note ? <span className="text-muted-foreground">{props.note}</span> : null}
    </div>
  );
}
