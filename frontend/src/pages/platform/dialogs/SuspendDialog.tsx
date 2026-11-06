import { TriangleAlertIcon } from "lucide-react";
import { useEffect, useId, useState } from "react";

import { usePlatformApi } from "../../../api/pending/PlatformApiProvider.tsx";
import type { SuspensionReason, TenantStateSnapshot, TenantSummary } from "../../../api/pending/platform.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { StaleStateError } from "../errors.ts";
import { REASON_LABELS } from "../format.ts";
import { ActionDialog } from "../ui/ActionDialog.tsx";
import { ConfirmByTyping, typedMatches } from "../ui/confirm-by-typing.tsx";
import { ImpactList } from "../ui/impact-list.tsx";

const REASONS: readonly SuspensionReason[] = ["non_payment", "security_or_abuse", "contract_terminated"];
const HINTS = {
  non_payment: "platform.suspend.hint.non_payment",
  security_or_abuse: "platform.suspend.hint.security_or_abuse",
  contract_terminated: "platform.suspend.hint.contract_terminated",
} as const;

/**
 * Suspends a tenant, or (`mode="reason"`) changes the reason of a suspension that is under way or
 * done. The three reasons are the API's; the tenant's name is typed for every one of them (decision
 * Q1), and ending the contract also needs a ticked box, because it is final: the tenant can
 * neither be resumed nor given another reason. Before it writes, the dialog reads the state again
 * and stops when the version is not the one it opened with (decision Q3).
 */
export function SuspendDialog(props: {
  tenant: TenantSummary;
  mode: "suspend" | "reason";
  open: boolean;
  onOpenChange(open: boolean): void;
  onDone(message: string): void;
  onStale(current: TenantStateSnapshot): void;
  returnFocus?: () => HTMLElement | null;
}) {
  const { t } = useI18n();
  const api = usePlatformApi();
  const { tenant, mode } = props;
  const [reason, setReason] = useState<SuspensionReason>(
    mode === "reason" && tenant.reason ? tenant.reason : "non_payment",
  );
  const [typed, setTyped] = useState("");
  const [understood, setUnderstood] = useState(false);
  const groupId = useId();
  const checkId = useId();

  useEffect(() => {
    if (props.open) {
      setReason(mode === "reason" && tenant.reason ? tenant.reason : "non_payment");
      setTyped("");
      setUnderstood(false);
    }
  }, [props.open, mode, tenant.reason]);

  const final = reason === "contract_terminated";
  const reasonText = t(REASON_LABELS[reason]);
  const unchanged = mode === "reason" && reason === tenant.reason;
  const ready = typedMatches(typed, tenant.name) && (!final || understood) && !unchanged;

  return (
    <ActionDialog
      open={props.open}
      onOpenChange={props.onOpenChange}
      title={
        mode === "reason"
          ? t("platform.suspend.title_reason", { name: tenant.name })
          : final
            ? t("platform.suspend.title_final", { name: tenant.name })
            : t("platform.suspend.title", { name: tenant.name })
      }
      description={t(mode === "reason" ? "platform.suspend.description_reason" : "platform.suspend.description")}
      audit={t("platform.suspend.audit", {
        reason: reasonText,
        from: tenant.stateVersion,
        to: tenant.stateVersion + 1,
      })}
      confirmLabel={
        mode === "reason"
          ? t("platform.suspend.confirm_reason")
          : final
            ? t("platform.suspend.confirm_final")
            : t("platform.suspend.confirm", { name: tenant.name })
      }
      pendingLabel={t("platform.dialog.working")}
      tone="destructive"
      confirmDisabled={!ready}
      returnFocus={props.returnFocus}
      onStale={props.onStale}
      run={async () => {
        const fresh = await api.getTenantState(tenant.id);
        if (fresh.stateVersion !== tenant.stateVersion) throw new StaleStateError(fresh);
        await api.setTenantState(tenant.id, {
          state: "suspended",
          reason,
          expectedStateVersion: tenant.stateVersion,
        });
      }}
      onDone={() =>
        props.onDone(
          t(mode === "reason" ? "platform.suspend.done_reason" : "platform.suspend.done", { name: tenant.name }),
        )
      }
    >
      <fieldset className="grid gap-2" aria-labelledby={`${groupId}-legend`}>
        <legend id={`${groupId}-legend`} className="text-sm font-medium">
          {t("platform.suspend.reason")}
        </legend>
        {REASONS.map((value) => (
          <label
            key={value}
            className={cn(
              "flex cursor-pointer items-start gap-2.5 rounded-md border px-3 py-2 text-sm",
              reason === value && "border-primary bg-accent",
            )}
          >
            <input
              type="radio"
              name={`${groupId}-reason`}
              value={value}
              checked={reason === value}
              onChange={() => setReason(value)}
              className="mt-0.5 size-4 accent-primary"
            />
            <span className="grid gap-0.5">
              <span className="font-medium">{t(REASON_LABELS[value])}</span>
              <span className="text-muted-foreground">{t(HINTS[value])}</span>
            </span>
          </label>
        ))}
      </fieldset>

      {final ? (
        <Alert variant="destructive" className="grid-cols-[1rem_1fr]">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertTitle>{t("platform.suspend.final_title")}</AlertTitle>
          <AlertDescription>
            <p>{t("platform.suspend.final_body")}</p>
          </AlertDescription>
        </Alert>
      ) : mode === "suspend" ? (
        <ImpactList
          title={t("platform.impact.title")}
          items={[
            t("platform.suspend.impact.members", { count: tenant.members.used }),
            t("platform.suspend.impact.gateways", { count: tenant.gateways.used }),
            t("platform.suspend.impact.processing"),
          ]}
        />
      ) : null}

      <ConfirmByTyping
        label={t("platform.suspend.type", { name: tenant.name })}
        expected={tenant.name}
        value={typed}
        onChange={setTyped}
      />

      {final ? (
        <div className="flex items-center gap-2 text-sm">
          <input
            id={checkId}
            type="checkbox"
            checked={understood}
            onChange={(event) => setUnderstood(event.target.checked)}
            className="size-4 accent-primary"
          />
          <label htmlFor={checkId}>{t("platform.suspend.understood")}</label>
        </div>
      ) : null}
      {unchanged ? <p className="text-sm text-muted-foreground">{t("platform.suspend.same_reason")}</p> : null}
    </ActionDialog>
  );
}
