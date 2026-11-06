import { cn } from "../../../lib/utils.ts";

/** A placeholder bar for a row that is on its way; decoration, so hidden from screen readers. */
export function Skeleton({ className }: { className?: string }) {
  return <span aria-hidden="true" className={cn("block h-3 animate-pulse rounded bg-muted", className)} />;
}
