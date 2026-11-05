import { CircleCheckIcon, CircleXIcon, ClockIcon, LoaderIcon } from "lucide-react";
import type { ReactNode } from "react";

import type { DocumentVersionInfo } from "../../api/pending/documents.ts";
import { isMessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { ProgressBar } from "./ProgressBar.tsx";

/**
 * What the AI can do with a version, as the badge and the lines under it (design.md 「列表」):
 * status is never shown by colour alone, each has an icon and words.
 */
export function VersionStatus({
  name,
  version,
  actions,
}: {
  /** The document's name, to label the progress bar. */
  name: string;
  version: DocumentVersionInfo;
  /** Offered under a failure: retry and upload a new version. */
  actions?: ReactNode;
}) {
  const { t } = useI18n();
  switch (version.status) {
    case "queued":
      return (
        <Badge icon={<ClockIcon aria-hidden="true" />}>{t("documents.status.queued")}</Badge>
      );
    case "processing": {
      const total = version.page_count;
      return (
        <div className="grid justify-items-start gap-1.5">
          <Badge icon={<LoaderIcon aria-hidden="true" className="animate-spin" />}>
            {total === null
              ? t("documents.status.processing")
              : t("documents.status.processing_pages", { done: version.processed_pages, total })}
          </Badge>
          <ProgressBar
            label={t("documents.status.progress_label", { name })}
            {...(total === null ? {} : { value: version.processed_pages, max: total })}
            className="max-w-40"
          />
          <p className="text-xs text-muted-foreground">{t("documents.status.processing_note")}</p>
        </div>
      );
    }
    case "ready":
      return (
        <Badge tone="ok" icon={<CircleCheckIcon aria-hidden="true" className="text-success" />}>
          {t("documents.status.ready")}
        </Badge>
      );
    case "failed":
      return (
        <div className="grid justify-items-start gap-1">
          <Badge tone="danger" icon={<CircleXIcon aria-hidden="true" />}>
            {t("documents.status.failed")}
          </Badge>
          <p className="text-xs">{failureText(version, t)}</p>
          {actions}
        </div>
      );
  }
}

function failureText(version: DocumentVersionInfo, t: ReturnType<typeof useI18n>["t"]): string {
  const failure = version.failure;
  const reason =
    failure !== null && failure.page !== null && isMessageId(failure.message_key)
      ? t(failure.message_key, { page: failure.page })
      : t("documents.failure.generic");
  return version.processed_pages > 0
    ? `${reason} ${t("documents.failure.others_ready", { count: version.processed_pages })}`
    : reason;
}

function Badge({
  icon,
  tone,
  children,
}: {
  icon: ReactNode;
  tone?: "ok" | "danger";
  children: ReactNode;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-xs font-medium [&_svg]:size-3.5 [&_svg]:shrink-0",
        tone === "danger" && "border-destructive/50 text-destructive",
        tone === undefined && "bg-muted text-foreground",
      )}
    >
      {icon}
      {children}
    </span>
  );
}
