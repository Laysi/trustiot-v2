import { ScrollTextIcon } from "lucide-react";
import type { ReactNode } from "react";

/** 「將記入操作紀錄」: which fields the backend writes to the operation log for this action. */
export function AuditNote({ children }: { children: ReactNode }) {
  return (
    <p className="flex items-start gap-2 rounded-md bg-muted px-3 py-2 text-xs text-muted-foreground">
      <ScrollTextIcon aria-hidden="true" className="mt-0.5 size-3.5 shrink-0" />
      <span>{children}</span>
    </p>
  );
}
