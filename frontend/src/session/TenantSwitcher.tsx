import { Building2Icon, ChevronsUpDownIcon } from "lucide-react";
import { useState } from "react";

import { Button } from "../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../components/ui/dropdown-menu.tsx";
import { Spinner } from "../components/ui/spinner.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { useSession } from "./SessionProvider.tsx";
import { ROLE_LABELS, tenantName } from "./tenant-name.ts";

/**
 * The active tenant in the top bar. With more than one tenant it opens a menu of them; the
 * active one is checked, and choosing another switches the session (SessionProvider). It is
 * disabled while any switch is in progress, this one's or a tenant list's on the page.
 */
export function TenantSwitcher() {
  const { t } = useI18n();
  const { session, switchTenant, switching } = useSession();
  const [failed, setFailed] = useState(false);

  if (session === null || session.tenants.length === 0) {
    return null;
  }
  const active = session.activeTenant;
  const activeName = active ? tenantName(active, t) : t("tenant_switcher.none");
  if (session.tenants.length === 1 && active !== null) {
    return (
      <p className="flex min-w-0 items-center gap-2 text-sm">
        <Building2Icon className="size-4 shrink-0 text-muted-foreground" />
        <span className="sr-only">{t("tenant_switcher.label")}: </span>
        <span className="truncate font-medium">{activeName}</span>
      </p>
    );
  }

  const choose = async (tenantId: string) => {
    if (tenantId === active?.id || switching) return;
    setFailed(false);
    try {
      await switchTenant(tenantId);
    } catch {
      setFailed(true);
    }
  };

  return (
    <div className="relative min-w-0">
      <DropdownMenu onOpenChange={(open) => open && setFailed(false)}>
        <DropdownMenuTrigger asChild>
          <Button variant="ghost" size="sm" disabled={switching} className="max-w-[16rem] min-w-0 gap-2">
            {switching ? <Spinner /> : <Building2Icon className="text-muted-foreground" />}
            <span className="sr-only">{t("tenant_switcher.label")}: </span>
            <span className="truncate">{activeName}</span>
            <ChevronsUpDownIcon className="text-muted-foreground" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="w-64">
          <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">
            {t("tenant_switcher.menu_label")}
          </DropdownMenuLabel>
          <DropdownMenuSeparator />
          <DropdownMenuRadioGroup value={active?.id ?? ""} onValueChange={(tenantId) => void choose(tenantId)}>
            {session.tenants.map((tenant) => (
              <DropdownMenuRadioItem key={tenant.id} value={tenant.id}>
                <span className="grid min-w-0">
                  <span className="truncate">{tenantName(tenant, t)}</span>
                  <span className="text-xs text-muted-foreground">{t(ROLE_LABELS[tenant.role])}</span>
                </span>
              </DropdownMenuRadioItem>
            ))}
          </DropdownMenuRadioGroup>
        </DropdownMenuContent>
      </DropdownMenu>
      {failed ? (
        <p
          role="alert"
          className="absolute top-full left-0 z-40 mt-1 w-64 rounded-md border border-destructive/50 bg-popover px-3 py-2 text-sm text-destructive shadow-md"
        >
          {t("tenant_switcher.switch_failed")}
        </p>
      ) : null}
    </div>
  );
}
