import type { ReactNode } from "react";

import { cn } from "../../lib/utils.ts";

/**
 * A small label of a state (docs/design/chat-dashboards/design.md): an icon and the words, never
 * color alone. `warning` is the state worth a second look, such as an older version.
 */
export function Tag({
  tone = "neutral",
  icon,
  children,
}: {
  tone?: "neutral" | "warning";
  icon?: ReactNode;
  children: ReactNode;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-sm px-2 py-0.5 text-xs font-medium whitespace-nowrap [&_svg]:size-3.5 [&_svg]:shrink-0",
        tone === "warning" ? "bg-warning-soft text-warning" : "bg-secondary text-secondary-foreground",
      )}
    >
      {icon}
      {children}
    </span>
  );
}
