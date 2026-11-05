import { CircleCheckIcon, CircleXIcon, FileTextIcon, XIcon } from "lucide-react";
import type { ReactNode } from "react";

import { ApiError } from "../../api/errors.ts";
import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { formatBytes } from "./format.ts";
import { ProgressBar } from "./ProgressBar.tsx";
import type { UploadItem } from "./useUploadQueue.ts";

/** The refusal of the capacity entitlement (docs/product/plans.md), the one upload error with its own text. */
const LIMIT_REACHED = "entitlement.limit_reached";

/**
 * One file of the upload queue: its name (as text, never markup), the state it is in with the
 * progress, the reason when it was refused or failed, and the actions that fit. The upload dialog
 * and the list's "uploading in the background" section share it (the queue component of
 * design.md 「Laysi/trustiot-v2#200 對話附件」).
 */
export function UploadRow({
  item,
  onRemove,
  onRetry,
  children,
}: {
  item: UploadItem;
  onRemove(id: number): void;
  onRetry(id: number): void;
  /** Extra controls under the row (the choice between a new version and a new document). */
  children?: ReactNode;
}) {
  const { t, errorMessage, locale } = useI18n();
  const name = item.file.name;
  const failed = item.state === "rejected" || item.state === "failed";
  const percent = Math.round(item.fraction * 100);
  return (
    <li className={cn("grid gap-1.5 rounded-md border px-3 py-2 text-sm", failed && "border-destructive/50")}>
      <div className="flex items-center gap-2">
        {item.state === "done" ? (
          <CircleCheckIcon aria-hidden="true" className="size-4 shrink-0 text-success" />
        ) : failed ? (
          <CircleXIcon aria-hidden="true" className="size-4 shrink-0 text-destructive" />
        ) : (
          <FileTextIcon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
        )}
        <span className="min-w-0 flex-1 wrap-anywhere">
          {name} <span className="text-xs text-muted-foreground">{formatBytes(item.file.size, locale)}</span>
        </span>
        {item.state === "uploading" ? (
          <span className="text-xs text-muted-foreground tabular-nums">{percent}%</span>
        ) : null}
        {item.state !== "done" ? (
          <Button
            variant="ghost"
            size="icon"
            className="size-8"
            aria-label={t(item.state === "uploading" ? "documents.upload.cancel_named" : "documents.upload.remove_named", {
              name,
            })}
            onClick={() => onRemove(item.id)}
          >
            <XIcon aria-hidden="true" />
          </Button>
        ) : null}
      </div>
      {item.state === "uploading" ? (
        <ProgressBar label={t("documents.upload.progress_label", { name })} value={percent} />
      ) : null}
      {item.state === "done" ? <p className="text-xs">{t("documents.upload.done")}</p> : null}
      {item.state === "rejected" ? (
        <p className="text-xs">
          {item.rejection === "not_pdf"
            ? t("documents.upload.not_pdf")
            : t("documents.upload.over_capacity", { remaining: formatBytes(item.remainingBytes ?? 0, locale) })}
        </p>
      ) : null}
      {item.state === "failed" ? (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <p role="status" className="text-xs">
            {item.error instanceof ApiError && item.error.body?.message_key === LIMIT_REACHED
              ? t("documents.upload.over_capacity_refused")
              : t("documents.upload.failed", { reason: errorMessage(item.error) })}
          </p>
          <Button variant="link" size="sm" className="h-auto text-xs" onClick={() => onRetry(item.id)}>
            {t("page.action.retry")}
          </Button>
        </div>
      ) : null}
      {children}
    </li>
  );
}
