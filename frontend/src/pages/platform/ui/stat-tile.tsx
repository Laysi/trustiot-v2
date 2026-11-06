import type { ReactNode } from "react";

import { focusRing } from "../../../components/ui/button.tsx";
import { cn } from "../../../lib/utils.ts";

/**
 * One figure of a summary row. With `onClick` it is a button that applies the filter the figure
 * stands for (`pressed` marks the one applied).
 */
export function StatTile({
  label,
  value,
  note,
  onClick,
  pressed,
}: {
  label: ReactNode;
  value: ReactNode;
  note?: ReactNode;
  onClick?: () => void;
  pressed?: boolean;
}) {
  const body = (
    <>
      <span className="flex items-center gap-1.5 text-sm text-muted-foreground">{label}</span>
      <span className="text-2xl font-semibold tabular-nums">{value}</span>
      {note ? <span className="text-xs text-muted-foreground">{note}</span> : null}
    </>
  );
  const classes = "grid min-w-0 gap-0.5 rounded-lg border bg-card p-4 text-left text-card-foreground";
  if (onClick === undefined) return <div className={classes}>{body}</div>;
  return (
    <button
      type="button"
      aria-pressed={pressed ?? false}
      onClick={onClick}
      className={cn(classes, "hover:bg-accent", pressed && "border-primary bg-accent", focusRing)}
    >
      {body}
    </button>
  );
}
