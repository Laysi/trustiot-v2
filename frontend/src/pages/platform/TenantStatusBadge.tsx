import { BanIcon, CircleCheckIcon, LoaderCircleIcon, LockIcon } from "lucide-react";

import type { SuspensionReason, TenantStatus } from "../../api/pending/platform.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { Badge } from "./ui/badge.tsx";

/**
 * The state of a tenant as a badge with an icon and words (design.md, 「各畫面的重點」): active,
 * processing (a suspension certs has not confirmed, never shown as suspended), suspended, and
 * suspended for good (the contract was terminated), and deleted.
 */
export function TenantStatusBadge({ status, reason }: { status: TenantStatus; reason: SuspensionReason | null }) {
  const { t } = useI18n();
  if (status === "active") {
    return (
      <Badge tone="success">
        <CircleCheckIcon aria-hidden="true" />
        {t("platform.status.active")}
      </Badge>
    );
  }
  if (status === "processing") {
    return (
      <Badge tone="warning">
        <LoaderCircleIcon aria-hidden="true" className="animate-spin" />
        {t("platform.status.processing")}
      </Badge>
    );
  }
  if (status === "deleted") {
    return (
      <Badge tone="danger">
        <LockIcon aria-hidden="true" />
        {t("platform.status.deleted")}
      </Badge>
    );
  }
  if (reason === "contract_terminated") {
    return (
      <Badge tone="danger">
        <LockIcon aria-hidden="true" />
        {t("platform.status.final")}
      </Badge>
    );
  }
  return (
    <Badge tone="danger">
      <BanIcon aria-hidden="true" />
      {t("platform.status.suspended")}
    </Badge>
  );
}
