import { CircleCheckIcon, CircleHelpIcon, CloudOffIcon, TriangleAlertIcon } from "lucide-react";

import type { GatewayRow, Health } from "../../../api/pending/platform.ts";
import { useI18n } from "../../../i18n/useI18n.ts";
import { Badge } from "../ui/badge.tsx";

/** The health of a bound gateway: an icon and words, and for a degraded one what is behind. */
export function GatewayHealth({ gateway }: { gateway: GatewayRow }) {
  const { t, formatNumber } = useI18n();
  if (gateway.health === null) {
    return <span className="text-muted-foreground">{t("platform.gateways.health.none")}</span>;
  }
  return (
    <span className="grid justify-items-start gap-1">
      <HealthBadge health={gateway.health} />
      {gateway.health === "degraded" && gateway.backlog !== null ? (
        <span className="text-xs text-muted-foreground">
          {t("platform.gateways.backlog", { count: formatNumber(gateway.backlog) })}
        </span>
      ) : null}
    </span>
  );
}

export function HealthBadge({ health }: { health: Health }) {
  const { t } = useI18n();
  if (health === "online") {
    return (
      <Badge tone="success">
        <CircleCheckIcon aria-hidden="true" />
        {t("platform.gateways.health.online")}
      </Badge>
    );
  }
  if (health === "degraded") {
    return (
      <Badge tone="warning">
        <TriangleAlertIcon aria-hidden="true" />
        {t("platform.gateways.health.degraded")}
      </Badge>
    );
  }
  if (health === "offline") {
    return (
      <Badge tone="danger">
        <CloudOffIcon aria-hidden="true" />
        {t("platform.gateways.health.offline")}
      </Badge>
    );
  }
  return (
    <Badge tone="outline">
      <CircleHelpIcon aria-hidden="true" />
      {t("platform.gateways.health.unknown")}
    </Badge>
  );
}
