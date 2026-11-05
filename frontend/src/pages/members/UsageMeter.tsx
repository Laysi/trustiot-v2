import { cn } from "../../lib/utils.ts";

/**
 * Seats used of the plan's limit: the numbers are text, the bar is only a help. It is an SVG whose
 * widths are attributes, since the CSP allows no `style` attribute (frontend/README.md).
 */
export function UsageMeter({
  label,
  used,
  limit,
  note,
  ariaLabel,
}: {
  label: string;
  used: number;
  /** `null`: the plan has no limit, so there is no bar. */
  limit: number | null;
  note: string;
  ariaLabel: string;
}) {
  const full = limit !== null && used >= limit;
  const percent = limit === null || limit === 0 ? 0 : Math.min(100, (used / limit) * 100);
  return (
    <div className="grid max-w-80 gap-1.5">
      <div className="flex items-baseline justify-between gap-3 text-sm">
        <span>{label}</span>
        <span className="font-medium tabular-nums">{limit === null ? used : `${used} / ${limit}`}</span>
      </div>
      {limit === null ? null : (
        <svg role="img" aria-label={ariaLabel} className="h-2 w-full" viewBox="0 0 100 2" preserveAspectRatio="none">
          <rect width="100" height="2" rx="1" className="fill-muted" />
          <rect width={percent} height="2" rx="1" className={cn(full ? "fill-warning" : "fill-brand")} />
        </svg>
      )}
      <p className="text-xs text-muted-foreground">{note}</p>
    </div>
  );
}
