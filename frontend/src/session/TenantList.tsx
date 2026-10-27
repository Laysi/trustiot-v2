import { Building2Icon, ChevronRightIcon } from "lucide-react";
import { useState } from "react";

import { Alert, AlertDescription } from "../components/ui/alert.tsx";
import { focusRing } from "../components/ui/button.tsx";
import { Spinner } from "../components/ui/spinner.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { cn } from "../lib/utils.ts";
import { useSession } from "./SessionProvider.tsx";
import type { TenantMembership } from "./session.ts";
import { ROLE_LABELS, tenantName } from "./tenant-name.ts";

/**
 * The user's tenants as a list of buttons, each with the user's role there; choosing one makes it
 * the session's tenant. Used where the session has to land in a tenant before anything else. The
 * buttons are disabled while any switch is in progress, including one from the top bar.
 */
export function TenantList({ tenants, label }: { tenants: readonly TenantMembership[]; label: string }) {
  const { t, errorMessage } = useI18n();
  const { switchTenant, switching } = useSession();
  const [opening, setOpening] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);

  const choose = async (tenantId: string) => {
    if (switching) return;
    setOpening(tenantId);
    setError(null);
    try {
      await switchTenant(tenantId);
    } catch (cause) {
      setError(cause);
    } finally {
      setOpening(null);
    }
  };

  return (
    <div className="grid w-full gap-3">
      <ul aria-label={label} className="grid gap-2">
        {tenants.map((tenant) => (
          <li key={tenant.id}>
            <button
              type="button"
              disabled={switching}
              onClick={() => void choose(tenant.id)}
              className={cn(
                "flex w-full items-center gap-3 rounded-lg border bg-card px-4 py-3 text-left text-card-foreground transition-colors hover:bg-accent disabled:opacity-60",
                focusRing,
              )}
            >
              <span className="flex size-9 shrink-0 items-center justify-center rounded-md bg-primary/10 text-primary">
                <Building2Icon className="size-5" />
              </span>
              <span className="grid min-w-0 flex-1">
                <span className="truncate font-medium">{tenantName(tenant, t)}</span>
                <span className="text-sm text-muted-foreground">{t(ROLE_LABELS[tenant.role])}</span>
              </span>
              {opening === tenant.id ? (
                <Spinner />
              ) : (
                <ChevronRightIcon className="size-4 shrink-0 text-muted-foreground" />
              )}
            </button>
          </li>
        ))}
      </ul>
      {error ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{t("tenant_switcher.switch_failed")}</p>
            <p>{errorMessage(error)}</p>
          </AlertDescription>
        </Alert>
      ) : null}
    </div>
  );
}
