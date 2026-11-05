import { CircleDashedIcon, CircleIcon, CircleSlashIcon, DiamondIcon } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "../../lib/utils.ts";

export type BadgeTone = "online" | "offline" | "unknown" | "attention";

/**
 * A state as a shape, a word and a colour, never the colour alone (docs/design/tenant-admin/
 * design.md, 「PresenceBadge」): a filled circle is online, a circle with a slash offline, a
 * dashed circle waiting for the first connection, a diamond something to look at (a configuration
 * being applied).
 */
export function StateMark({ tone, children, className }: { tone: BadgeTone; children: ReactNode; className?: string }) {
  const Icon =
    tone === "online"
      ? CircleIcon
      : tone === "offline"
        ? CircleSlashIcon
        : tone === "attention"
          ? DiamondIcon
          : CircleDashedIcon;
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 font-medium",
        tone === "online" && "text-success",
        tone === "offline" && "text-destructive",
        tone === "attention" && "text-warning",
        tone === "unknown" && "text-muted-foreground",
        className,
      )}
    >
      <Icon aria-hidden="true" className={cn("size-3.5 shrink-0", tone === "online" && "fill-current")} />
      {children}
    </span>
  );
}
