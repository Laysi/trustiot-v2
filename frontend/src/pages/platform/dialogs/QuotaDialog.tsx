import { TriangleAlertIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { usePlatformApi } from "../../../api/pending/PlatformApiProvider.tsx";
import type { QuotaChange, TenantSummary } from "../../../api/pending/platform.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import { FormField } from "../../../components/ui/form-field.tsx";
import { controlClasses, Input } from "../../../components/ui/input.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { formatUsd, PLAN_LABELS } from "../format.ts";
import { ActionDialog } from "../ui/ActionDialog.tsx";
import { ChangePreview } from "../ui/change-preview.tsx";

/** A whole number of 0 or more, or `null` for text that is not one. */
function wholeNumber(text: string): number | null {
  return /^\d{1,9}$/.test(text.trim()) ? Number(text.trim()) : null;
}

/** An amount of 0 or more with up to two decimals, or `null`. */
function amount(text: string): number | null {
  return /^\d{1,7}(\.\d{1,2})?$/.test(text.trim()) ? Number(text.trim()) : null;
}

/**
 * Changes the gateway limit and this month's AI raise of a tenant (decision Q5: the reason is
 * required and goes to the operation log). It shows the values before and after, applies only what
 * changed, and warns when a new limit is below what the tenant already uses: existing gateways keep
 * working, but nothing new can be bound.
 */
export function QuotaDialog(props: {
  tenant: TenantSummary;
  open: boolean;
  onOpenChange(open: boolean): void;
  onDone(message: string): void;
  returnFocus?: () => HTMLElement | null;
}) {
  const { t, locale } = useI18n();
  const api = usePlatformApi();
  const { tenant } = props;
  const currentLimit = tenant.gateways.limit === "unlimited" ? "" : String(tenant.gateways.limit);
  const [gateways, setGateways] = useState(currentLimit);
  const [extra, setExtra] = useState(String(tenant.ai.extraUsd));
  const [reason, setReason] = useState("");
  // Takes the override off, so the limit is the plan's again (sent as `maxGateways: null`).
  const [usePlanDefault, setUsePlanDefault] = useState(false);

  useEffect(() => {
    if (props.open) {
      setGateways(currentLimit);
      setExtra(String(tenant.ai.extraUsd));
      setReason("");
      setUsePlanDefault(false);
    }
  }, [props.open, currentLimit, tenant.ai.extraUsd]);

  // An empty gateway field keeps a limit that is "unlimited"; it is not a number for any other.
  const gatewayValue = wholeNumber(gateways);
  const gatewayInvalid = usePlanDefault
    ? false
    : gateways.trim() === ""
      ? tenant.gateways.limit !== "unlimited"
      : gatewayValue === null;
  const planLimit = tenant.planDefaults.maxGateways;
  const newLimit = usePlanDefault ? planLimit : gatewayValue;
  const extraValue = amount(extra);
  const extraInvalid = extraValue === null;
  const gatewaysChanged = newLimit !== null && newLimit !== tenant.gateways.limit;
  const extraChanged = extraValue !== null && extraValue !== tenant.ai.extraUsd;
  const changes = Number(gatewaysChanged) + Number(extraChanged);
  const valid = !gatewayInvalid && !extraInvalid && reason.trim() !== "";
  const below = gatewaysChanged && typeof newLimit === "number" && newLimit < tenant.gateways.used;

  const gatewayError = gatewayInvalid ? t("platform.quota.gateways_invalid") : undefined;
  const extraError = extraInvalid ? t("platform.quota.extra_invalid") : undefined;
  const defaultGateways =
    tenant.planDefaults.maxGateways === "unlimited" ? t("platform.limit.unlimited") : tenant.planDefaults.maxGateways;

  return (
    <ActionDialog
      open={props.open}
      onOpenChange={props.onOpenChange}
      className="sm:max-w-xl"
      title={t("platform.quota.title", { name: tenant.name })}
      description={t("platform.quota.description", { plan: t(PLAN_LABELS[tenant.plan]) })}
      audit={t("platform.quota.audit")}
      confirmLabel={t("platform.quota.confirm", { count: changes })}
      pendingLabel={t("platform.dialog.working")}
      confirmDisabled={!valid || changes === 0}
      returnFocus={props.returnFocus}
      run={() => {
        const change: QuotaChange = { reason: reason.trim() };
        if (gatewaysChanged) change.maxGateways = usePlanDefault ? null : gatewayValue;
        if (extraChanged) change.aiExtraUsd = extraValue;
        return api.adjustQuota(tenant.id, change);
      }}
      onDone={() => props.onDone(t("platform.quota.done", { name: tenant.name }))}
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField
          label={t("platform.quota.gateways")}
          description={t("platform.quota.gateways_hint", { used: tenant.gateways.used, default: defaultGateways })}
          error={gatewayError}
        >
          {(control) => (
            <Input
              {...control}
              inputMode="numeric"
              value={usePlanDefault ? "" : gateways}
              disabled={usePlanDefault}
              onChange={(event) => setGateways(event.target.value)}
            />
          )}
        </FormField>
        <FormField
          label={t("platform.quota.extra")}
          description={t("platform.quota.extra_hint", {
            plan: formatUsd(tenant.ai.planLimitUsd, locale),
          })}
          error={extraError}
        >
          {(control) => (
            <div className="flex items-center gap-2">
              <span aria-hidden="true" className="text-sm text-muted-foreground">
                US$
              </span>
              <Input
                {...control}
                inputMode="decimal"
                value={extra}
                onChange={(event) => setExtra(event.target.value)}
              />
            </div>
          )}
        </FormField>
      </div>
      <label className="flex items-start gap-2 text-sm">
        <input
          type="checkbox"
          className="mt-0.5 size-4 accent-primary"
          checked={usePlanDefault}
          onChange={(event) => setUsePlanDefault(event.target.checked)}
        />
        <span>{t("platform.quota.use_default", { default: defaultGateways })}</span>
      </label>
      <FormField label={t("platform.quota.reason")} description={t("platform.quota.reason_hint")}>
        {(control) => (
          <textarea
            {...control}
            rows={2}
            value={reason}
            maxLength={500}
            className={cn(controlClasses, "h-auto min-h-16 py-2")}
            onChange={(event) => setReason(event.target.value)}
          />
        )}
      </FormField>

      {below ? (
        <Alert className="grid-cols-[1rem_1fr] border-warning/50 bg-warning-soft text-foreground">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertTitle>{t("platform.quota.below_title", { limit: newLimit, used: tenant.gateways.used })}</AlertTitle>
          <AlertDescription className="text-foreground">
            <p>{t("platform.quota.below_body", { limit: newLimit, used: tenant.gateways.used })}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      {changes > 0 ? (
        <fieldset aria-label={t("platform.quota.changes")} className="m-0 grid min-w-0 gap-2 border-0 p-0">
          {gatewaysChanged ? (
            <ChangePreview
              label={t("platform.quota.preview_gateways")}
              from={tenant.gateways.limit === "unlimited" ? t("platform.limit.unlimited") : tenant.gateways.limit}
              to={newLimit === "unlimited" ? t("platform.limit.unlimited") : newLimit}
            />
          ) : null}
          {extraChanged ? (
            <ChangePreview
              label={t("platform.quota.preview_ai", { until: tenant.ai.extraUntil ?? "" })}
              from={formatUsd(tenant.ai.planLimitUsd + tenant.ai.extraUsd, locale)}
              to={formatUsd(tenant.ai.planLimitUsd + (extraValue ?? 0), locale)}
            />
          ) : null}
        </fieldset>
      ) : null}
    </ActionDialog>
  );
}
