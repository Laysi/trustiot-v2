import { CircleAlertIcon, CircleCheckIcon, LoaderCircleIcon } from "lucide-react";

import type { ReportStatus } from "../../api/pending/reports.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";

/**
 * A report's state as an icon and a word, never a color alone (design 「顏色與圖示」): done has
 * the success icon, generating the muted one, failed the destructive text and border.
 */
export function StatusBadge({ status }: { status: ReportStatus }) {
  const { t } = useI18n();
  const Icon = status === "ready" ? CircleCheckIcon : status === "generating" ? LoaderCircleIcon : CircleAlertIcon;
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs font-medium whitespace-nowrap",
        status === "failed" ? "border-destructive text-destructive" : "bg-muted text-foreground",
      )}
    >
      <Icon
        aria-hidden="true"
        className={cn("size-3.5", status === "ready" && "text-success", status === "generating" && "animate-spin")}
      />
      {t(status === "ready" ? "reports.status.ready" : status === "generating" ? "reports.status.generating" : "reports.status.failed")}
    </span>
  );
}
