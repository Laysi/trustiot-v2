import { Building2Icon, UsersRoundIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Outlet } from "react-router";

import { Button } from "../components/ui/button.tsx";
import { Spinner } from "../components/ui/spinner.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { StatusPage } from "../pages/StatusPage.tsx";
import { useSession } from "./SessionProvider.tsx";
import { SignOutButton } from "./SignOutButton.tsx";
import type { TenantMembership } from "./session.ts";
import { TenantList } from "./TenantList.tsx";
import { TenantSuspendedPage } from "./TenantSuspendedPage.tsx";
import { tenantName } from "./tenant-name.ts";

/**
 * In front of every page that works on a tenant's data. A session in no tenant (a first sign-in,
 * or one whose tenant the user has left) lands in one first: the only tenant automatically, or
 * the one the user picks; a user with no tenant is told why there is nothing to show. A tenant
 * whose requests come back suspended gets the suspension page instead of its pages.
 *
 * The pages belong to one tenant: when the session moves to another (here, or in another tab),
 * they start afresh, so nothing typed into a page of the previous tenant can be sent in the new
 * one (Laysi/trustiot-v2#678).
 */
export function TenantGate() {
  const { session, suspended } = useSession();
  if (session === null) {
    return null;
  }
  if (session.activeTenant !== null) {
    return suspended ? <TenantSuspendedPage /> : <Outlet key={session.activeTenant.id} />;
  }
  const [only, ...others] = session.tenants;
  if (only === undefined) {
    return <NoTenantPage />;
  }
  if (others.length === 0) {
    return <OpenOnlyTenant tenant={only} />;
  }
  return <ChooseTenantPage tenants={session.tenants} />;
}

function ChooseTenantPage({ tenants }: { tenants: readonly TenantMembership[] }) {
  const { t } = useI18n();
  return (
    <StatusPage title={t("tenant_gate.choose.title")} icon={<Building2Icon />}>
      <p>{t("tenant_gate.choose.body")}</p>
      <TenantList tenants={tenants} label={t("tenant_switcher.menu_label")} />
    </StatusPage>
  );
}

function NoTenantPage() {
  const { t } = useI18n();
  const { reload } = useSession();
  return (
    <StatusPage
      title={t("tenant_gate.none.title")}
      icon={<UsersRoundIcon />}
      actions={
        <>
          <Button onClick={reload}>{t("tenant_gate.check_again")}</Button>
          <SignOutButton />
        </>
      }
    >
      <p>{t("tenant_gate.none.body")}</p>
    </StatusPage>
  );
}

/** One tenant: nothing to choose, so the session switches to it at once (once per attempt). */
function OpenOnlyTenant({ tenant }: { tenant: TenantMembership }) {
  const { t, errorMessage } = useI18n();
  const { switchTenant } = useSession();
  const [attempt, setAttempt] = useState(0);
  const [error, setError] = useState<unknown>(null);
  const started = useRef<number | null>(null);
  const name = tenantName(tenant, t);

  useEffect(() => {
    if (started.current === attempt) return;
    started.current = attempt;
    switchTenant(tenant.id).catch(setError);
  }, [attempt, switchTenant, tenant.id]);

  if (error !== null) {
    return (
      <StatusPage
        title={t("tenant_gate.open_failed", { name })}
        icon={<Building2Icon />}
        actions={
          <>
            <Button
              onClick={() => {
                setError(null);
                setAttempt((count) => count + 1);
              }}
            >
              {t("tenant_gate.retry")}
            </Button>
            <SignOutButton />
          </>
        }
      >
        <p>{errorMessage(error)}</p>
      </StatusPage>
    );
  }
  return (
    <div className="flex justify-center py-16">
      <Spinner label={t("tenant_gate.opening", { name })} />
    </div>
  );
}
