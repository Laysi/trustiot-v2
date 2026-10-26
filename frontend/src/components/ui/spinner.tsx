import { LoaderCircleIcon } from "lucide-react";

import { cn } from "../../lib/utils.ts";

/**
 * A spinning circle. With `label` it is announced as a status (a whole page loading); without one
 * it is decoration next to text that already says what is happening ("Sending…").
 */
export function Spinner({ className, label }: { className?: string; label?: string }) {
  const icon = <LoaderCircleIcon className={cn("size-4 animate-spin", className)} />;
  if (label === undefined) {
    return icon;
  }
  return (
    <span role="status" className="inline-flex items-center gap-2 text-sm text-muted-foreground">
      {icon}
      {label}
    </span>
  );
}
