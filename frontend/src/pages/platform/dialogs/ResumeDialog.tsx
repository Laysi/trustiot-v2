import { usePlatformApi } from "../../../api/pending/PlatformApiProvider.tsx";
import type { TenantStateSnapshot, TenantSummary } from "../../../api/pending/platform.ts";
import { useI18n } from "../../../i18n/useI18n.ts";
import { StaleStateError } from "../errors.ts";
import { REASON_LABELS } from "../format.ts";
import { useUserNames } from "../queries.ts";
import { ActionDialog } from "../ui/ActionDialog.tsx";
import { ImpactList } from "../ui/impact-list.tsx";

/**
 * Resumes a suspended tenant. A resumption is "normal" at once, without waiting for the services
 * downstream, and it can be undone by suspending again, so it asks for a confirmation but not for
 * the name (design.md, 「高風險操作的確認分級」). The dialog shows the reason and time of the
 * suspension it undoes, and reads the state again before it writes (decision Q3).
 */
export function ResumeDialog(props: {
  tenant: TenantSummary;
  open: boolean;
  onOpenChange(open: boolean): void;
  onDone(message: string): void;
  onStale(current: TenantStateSnapshot): void;
  returnFocus?: () => HTMLElement | null;
}) {
  const { t, formatDateTime } = useI18n();
  const api = usePlatformApi();
  const { tenant } = props;
  const nameOf = useUserNames([tenant.changedByUserId]);
  const reason = tenant.reason ? t(REASON_LABELS[tenant.reason]) : "";
  return (
    <ActionDialog
      open={props.open}
      onOpenChange={props.onOpenChange}
      title={t("platform.resume.title", { name: tenant.name })}
      description={t("platform.resume.description")}
      audit={t("platform.resume.audit", {
        reason,
        from: tenant.stateVersion,
        to: tenant.stateVersion + 1,
      })}
      confirmLabel={t("platform.resume.confirm", { name: tenant.name })}
      pendingLabel={t("platform.dialog.working")}
      returnFocus={props.returnFocus}
      onStale={props.onStale}
      run={async () => {
        const fresh = await api.getTenantState(tenant.id);
        if (fresh.stateVersion !== tenant.stateVersion) throw new StaleStateError(fresh);
        await api.setTenantState(tenant.id, {
          state: "active",
          reason: null,
          expectedStateVersion: tenant.stateVersion,
        });
      }}
      onDone={() => props.onDone(t("platform.resume.done", { name: tenant.name }))}
    >
      <p className="rounded-md border px-3 py-2 text-sm">
        {t("platform.resume.current", {
          reason,
          time: formatDateTime(tenant.stateChangedAt),
          who: tenant.changedByUserId ? nameOf(tenant.changedByUserId) : "—",
        })}
      </p>
      <ImpactList
        title={t("platform.impact.title")}
        items={[
          t("platform.resume.impact.members", { count: tenant.members.used }),
          t("platform.resume.impact.gateways"),
          t("platform.resume.impact.config"),
        ]}
      />
    </ActionDialog>
  );
}
