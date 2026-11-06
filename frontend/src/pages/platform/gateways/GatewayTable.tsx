import { BanIcon, EllipsisIcon, Link2OffIcon, LinkIcon, ShieldOffIcon, WrenchIcon } from "lucide-react";
import { useRef } from "react";
import { Link } from "react-router";

import type { GatewayRow } from "../../../api/pending/platform.ts";
import { Button, focusRing } from "../../../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../../../components/ui/dropdown-menu.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { platformTenantPath } from "../paths.ts";
import { useNow } from "../queries.ts";
import { Badge } from "../ui/badge.tsx";
import { StackTable, Td, Th, Tr } from "../ui/stack-table.tsx";
import { formatAgo } from "./ago.ts";
import { GatewayHealth } from "./GatewayHealth.tsx";

export type GatewayAction = "bind" | "unbind" | "repair" | "revoke" | "decommission";

/**
 * The gateways as a table (docs/design/platform-admin/gateways.html): hardware ID, the tenant (or
 * the device pool, or "in repair"), health, when it last reported, when it was registered at the
 * factory and by which workstation, and the row's actions. A gateway in the pool has the bind
 * button in its row; the others a menu sorted by how destructive each action is, the irreversible
 * ones last behind a divider. `showTenant` is false on a tenant's own page.
 */
export function GatewayTable(props: {
  gateways: readonly GatewayRow[];
  showTenant: boolean;
  onAction(action: GatewayAction, gateway: GatewayRow, element: HTMLElement | null): void;
}) {
  const { t, locale, formatDateTime } = useI18n();
  const now = useNow(15_000);
  return (
    <StackTable
      caption={t("platform.gateways.caption", { count: props.gateways.length })}
      head={
        <>
          <Th>{t("platform.gateways.column.id")}</Th>
          {props.showTenant ? <Th>{t("platform.gateways.column.tenant")}</Th> : null}
          <Th>{t("platform.gateways.column.health")}</Th>
          <Th>{t("platform.gateways.column.last")}</Th>
          <Th>{props.showTenant ? t("platform.gateways.column.registered") : t("platform.gateways.column.bound")}</Th>
          <Th>
            <span className="sr-only">{t("platform.gateways.column.actions")}</span>
          </Th>
        </>
      }
    >
      {props.gateways.map((gateway) => (
        <Tr key={gateway.hardwareId}>
          <Td label={t("platform.gateways.column.id")} className="font-mono">
            {gateway.hardwareId}
          </Td>
          {props.showTenant ? (
            <Td label={t("platform.gateways.column.tenant")}>
              <TenantCell gateway={gateway} />
            </Td>
          ) : null}
          <Td label={t("platform.gateways.column.health")}>
            <GatewayHealth gateway={gateway} />
          </Td>
          <Td label={t("platform.gateways.column.last")}>
            {gateway.lastReportAt ? formatAgo(gateway.lastReportAt, now, locale) : "—"}
          </Td>
          <Td label={props.showTenant ? t("platform.gateways.column.registered") : t("platform.gateways.column.bound")}>
            {props.showTenant ? (
              <span className="grid justify-items-start gap-0.5">
                <span>{formatDateTime(gateway.registeredAt)}</span>
                <span className="text-xs text-muted-foreground">
                  {t("platform.gateways.workstation", { name: gateway.workstation })}
                </span>
              </span>
            ) : gateway.boundAt ? (
              formatDateTime(gateway.boundAt)
            ) : (
              "—"
            )}
          </Td>
          <Td label={t("platform.gateways.column.actions")} className="sm:text-right">
            <RowActions gateway={gateway} onAction={props.onAction} />
          </Td>
        </Tr>
      ))}
    </StackTable>
  );
}

function TenantCell({ gateway }: { gateway: GatewayRow }) {
  const { t } = useI18n();
  if (gateway.placement === "repair") {
    return (
      <Badge tone="warning">
        <WrenchIcon aria-hidden="true" />
        {t("platform.gateways.repair")}
      </Badge>
    );
  }
  if (gateway.tenant === null) return <span className="text-muted-foreground">{t("platform.gateways.pool")}</span>;
  return (
    <span className="grid justify-items-start gap-1">
      <Link
        to={platformTenantPath(gateway.tenant.id)}
        className={cn("rounded-sm underline-offset-4 hover:underline", focusRing)}
      >
        {gateway.tenant.name}
      </Link>
      {gateway.tenant.status === "processing" ? (
        <span className="text-xs text-muted-foreground">{t("platform.gateways.tenant_processing")}</span>
      ) : null}
    </span>
  );
}

function RowActions(props: {
  gateway: GatewayRow;
  onAction(action: GatewayAction, gateway: GatewayRow, element: HTMLElement | null): void;
}) {
  const { t } = useI18n();
  const { gateway } = props;
  const button = useRef<HTMLButtonElement>(null);
  const menuButton = useRef<HTMLButtonElement>(null);
  const menu = (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          ref={menuButton}
          variant="ghost"
          size="icon"
          aria-label={t("platform.gateways.actions_of", { id: gateway.hardwareId })}
        >
          <EllipsisIcon />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        {gateway.placement === "bound" ? (
          <>
            <DropdownMenuItem onSelect={() => props.onAction("unbind", gateway, menuButton.current)}>
              <Link2OffIcon />
              {t("platform.gateways.menu.unbind")}
            </DropdownMenuItem>
            <DropdownMenuItem onSelect={() => props.onAction("repair", gateway, menuButton.current)}>
              <WrenchIcon />
              {t("platform.gateways.menu.repair")}
            </DropdownMenuItem>
            <DropdownMenuSeparator />
          </>
        ) : null}
        <DropdownMenuItem variant="destructive" onSelect={() => props.onAction("revoke", gateway, menuButton.current)}>
          <ShieldOffIcon />
          {t("platform.gateways.menu.revoke")}
        </DropdownMenuItem>
        <DropdownMenuItem
          variant="destructive"
          onSelect={() => props.onAction("decommission", gateway, menuButton.current)}
        >
          <BanIcon />
          {t("platform.gateways.menu.decommission")}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
  if (gateway.placement === "pool") {
    // A gateway in the pool is bound from here; revoking and decommissioning work on it too.
    return (
      <div className="flex items-center gap-1">
        <Button
          ref={button}
          variant="outline"
          size="sm"
          aria-label={t("platform.gateways.bind_named", { id: gateway.hardwareId })}
          onClick={() => props.onAction("bind", gateway, button.current)}
        >
          <LinkIcon />
          {t("platform.gateways.bind")}
        </Button>
        {menu}
      </div>
    );
  }
  return menu;
}
