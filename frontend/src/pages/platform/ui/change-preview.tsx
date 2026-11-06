import { ArrowRightIcon } from "lucide-react";
import type { ReactNode } from "react";

/** Before and after of one value, so the administrator sees what the confirmation changes. */
export function ChangePreview({ label, from, to }: { label: ReactNode; from: ReactNode; to: ReactNode }) {
  return (
    <div className="grid gap-1 rounded-md border px-3 py-2 text-sm">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="flex flex-wrap items-center gap-2">
        <span className="text-muted-foreground">{from}</span>
        <ArrowRightIcon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        <span className="font-semibold">{to}</span>
      </span>
    </div>
  );
}
