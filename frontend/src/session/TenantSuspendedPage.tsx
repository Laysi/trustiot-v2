import { BanIcon } from "lucide-react";

import { useI18n } from "../i18n/useI18n.ts";
import { StatusPage } from "../pages/StatusPage.tsx";
import { useSession } from "./SessionProvider.tsx";
import { SignOutButton } from "./SignOutButton.tsx";
import { TenantList } from "./TenantList.tsx";
import { tenantName } from "./tenant-name.ts";

/**
 * A suspended tenant (`tenant_suspended`, 403): the API refuses its requests, so its pages are
 * replaced by this one until the user switches to another tenant. Replaces the old system's
 * check every 60 seconds (docs/requirements/dropped.md).
 */
export function TenantSuspendedPage() {
  const { t } = useI18n();
  const { session } = useSession();
  const active = session?.activeTenant ?? null;
  const others = session?.tenants.filter((tenant) => tenant.id !== active?.id) ?? [];
  return (
    <StatusPage
      title={t("page.tenant_suspended.title")}
      icon={<BanIcon />}
      actions={session ? <SignOutButton /> : null}
    >
      <p>
        {active
          ? t("page.tenant_suspended.named_body", { name: tenantName(active, t) })
          : t("page.tenant_suspended.body")}
      </p>
      {others.length > 0 ? (
        <>
          <p>{t("page.tenant_suspended.other_tenants")}</p>
          <TenantList tenants={others} label={t("tenant_switcher.menu_label")} />
        </>
      ) : null}
    </StatusPage>
  );
}
