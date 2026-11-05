import { useQuery, useQueryClient } from "@tanstack/react-query";
import { TriangleAlertIcon } from "lucide-react";
import { useEffect, useId, useState } from "react";
import { Link } from "react-router";

import { usePlatformApi } from "../../../api/pending/PlatformApiProvider.tsx";
import type { GatewayRow, RevocationReason } from "../../../api/pending/platform.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import { FormField } from "../../../components/ui/form-field.tsx";
import { controlClasses } from "../../../components/ui/input.tsx";
import { NativeSelect } from "../../../components/ui/native-select.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { isFull } from "../format.ts";
import { platformTenantPath } from "../paths.ts";
import { platformKey, tenantsKey } from "../queries.ts";
import { ActionDialog } from "../ui/ActionDialog.tsx";
import { ConfirmByTyping, typedMatches } from "../ui/confirm-by-typing.tsx";
import { ImpactList } from "../ui/impact-list.tsx";
import type { GatewayAction } from "./GatewayTable.tsx";

export interface GatewayDialogTarget {
  action: GatewayAction;
  /** The gateway as the page showed it when the person opened the dialog. */
  gateway: GatewayRow;
}

interface Common {
  gateway: GatewayRow;
  onOpenChange(open: boolean): void;
  onDone(message: string): void;
  returnFocus?: (() => HTMLElement | null) | undefined;
}

/** The last 4 characters of a hardware ID, which a confirmation that disconnects a gateway asks for. */
export function lastFour(hardwareId: string): string {
  return hardwareId.slice(-4);
}

/**
 * The dialogs of a gateway's actions, for the gateways page and a tenant's page. Each follows
 * design.md's grading: binding is a plain confirmation that shows the tenant's quota; unbinding and
 * sending in for repair disconnect the gateway, so they ask for the last 4 characters of the
 * hardware ID; revoking the certificates and decommissioning ask for the whole ID, and
 * decommissioning, which cannot be undone, also for a ticked box. After a change every back office
 * query is read again.
 */
export function GatewayDialogs(props: {
  target: GatewayDialogTarget | null;
  onClose(): void;
  onDone(message: string): void;
  returnFocus?: () => HTMLElement | null;
}) {
  const queryClient = useQueryClient();
  const { target } = props;
  if (target === null) return null;
  const common: Common = {
    gateway: target.gateway,
    onOpenChange: (open) => {
      if (!open) props.onClose();
    },
    onDone: (message) => {
      void queryClient.invalidateQueries({ queryKey: platformKey });
      props.onClose();
      props.onDone(message);
    },
    returnFocus: props.returnFocus,
  };
  switch (target.action) {
    case "bind":
      return <BindDialog {...common} />;
    case "unbind":
      return <UnbindDialog {...common} />;
    case "repair":
      return <RepairDialog {...common} />;
    case "revoke":
      return <RevokeDialog {...common} />;
    case "decommission":
      return <DecommissionDialog {...common} />;
  }
}

function BindDialog(props: Common) {
  const { t, formatNumber } = useI18n();
  const api = usePlatformApi();
  const { gateway } = props;
  const tenants = useQuery({ queryKey: tenantsKey, queryFn: ({ signal }) => api.listTenants(signal) });
  // A suspended tenant cannot take a gateway, so it is not offered.
  const options = (tenants.data ?? []).filter((tenant) => tenant.state === "active");
  const [tenantId, setTenantId] = useState("");
  const chosen = options.find((tenant) => tenant.id === tenantId) ?? options[0];
  const full = chosen ? isFull(chosen.gateways.used, chosen.gateways.limit) : false;
  return (
    <ActionDialog
      open
      onOpenChange={props.onOpenChange}
      title={t("platform.gateways.bind.title", { id: gateway.hardwareId })}
      description={t("platform.gateways.bind.description")}
      audit={t("platform.gateways.bind.audit")}
      confirmLabel={
        chosen ? t("platform.gateways.bind.confirm", { name: chosen.name }) : t("platform.gateways.bind.confirm_none")
      }
      pendingLabel={t("platform.dialog.working")}
      confirmDisabled={!chosen || full}
      returnFocus={props.returnFocus}
      run={() => api.bindGateway(gateway.hardwareId, (chosen as { id: string }).id)}
      onDone={() =>
        props.onDone(t("platform.gateways.bind.done", { id: gateway.hardwareId, name: chosen?.name ?? "" }))
      }
    >
      <FormField label={t("platform.gateways.bind.tenant")} description={t("platform.gateways.bind.tenant_hint")}>
        {(control) => (
          <NativeSelect {...control} value={chosen?.id ?? ""} onChange={(event) => setTenantId(event.target.value)}>
            {options.map((tenant) => (
              <option key={tenant.id} value={tenant.id}>
                {t("platform.gateways.bind.option", {
                  name: tenant.name,
                  used: formatNumber(tenant.gateways.used),
                  limit:
                    tenant.gateways.limit === "unlimited"
                      ? t("platform.limit.unlimited")
                      : formatNumber(tenant.gateways.limit),
                })}
              </option>
            ))}
          </NativeSelect>
        )}
      </FormField>
      {tenants.isError ? (
        <Alert variant="destructive">
          <AlertDescription>
            <p>{t("platform.gateways.bind.tenants_failed")}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      {full && chosen ? (
        <Alert className="grid-cols-[1rem_1fr] border-warning/50 bg-warning-soft text-foreground">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertTitle>
            {t("platform.gateways.bind.full_title", {
              name: chosen.name,
              used: chosen.gateways.used,
              limit: chosen.gateways.limit === "unlimited" ? "" : chosen.gateways.limit,
            })}
          </AlertTitle>
          <AlertDescription className="text-foreground">
            <p>{t("platform.gateways.bind.full_body")}</p>
            <Link className="text-sm underline underline-offset-4" to={platformTenantPath(chosen.id)}>
              {t("platform.gateways.bind.full_link")}
            </Link>
          </AlertDescription>
        </Alert>
      ) : null}
    </ActionDialog>
  );
}

/** A confirmation that asks for the last 4 characters of the hardware ID. */
function useLastFour(open: boolean, gateway: GatewayRow) {
  const [typed, setTyped] = useState("");
  useEffect(() => {
    if (open) setTyped("");
  }, [open]);
  return { typed, setTyped, ready: typedMatches(typed, lastFour(gateway.hardwareId)) };
}

function UnbindDialog(props: Common) {
  const { t, formatNumber } = useI18n();
  const api = usePlatformApi();
  const { gateway } = props;
  const confirm = useLastFour(true, gateway);
  const tenant = gateway.tenant?.name ?? "";
  return (
    <ActionDialog
      open
      onOpenChange={props.onOpenChange}
      title={t("platform.gateways.unbind.title", { id: gateway.hardwareId, tenant })}
      audit={t("platform.gateways.unbind.audit")}
      confirmLabel={t("platform.gateways.unbind.confirm")}
      pendingLabel={t("platform.dialog.working")}
      tone="destructive"
      confirmDisabled={!confirm.ready}
      returnFocus={props.returnFocus}
      run={() => api.unbindGateway(gateway.hardwareId)}
      onDone={() => props.onDone(t("platform.gateways.unbind.done", { id: gateway.hardwareId }))}
    >
      <ImpactList
        title={t("platform.impact.title")}
        items={[
          t("platform.gateways.unbind.impact.revoke"),
          t("platform.gateways.unbind.impact.pool"),
          t("platform.gateways.unbind.impact.card"),
          gateway.backlog !== null
            ? t("platform.gateways.unbind.impact.backlog", { count: formatNumber(gateway.backlog) })
            : t("platform.gateways.unbind.impact.backlog_unknown"),
        ]}
      />
      <ConfirmByTyping
        label={t("platform.gateways.type_last4", { value: lastFour(gateway.hardwareId) })}
        expected={lastFour(gateway.hardwareId)}
        value={confirm.typed}
        onChange={confirm.setTyped}
      />
    </ActionDialog>
  );
}

function RepairDialog(props: Common) {
  const { t } = useI18n();
  const api = usePlatformApi();
  const { gateway } = props;
  const confirm = useLastFour(true, gateway);
  const [note, setNote] = useState("");
  return (
    <ActionDialog
      open
      onOpenChange={props.onOpenChange}
      title={t("platform.gateways.repair.title", { id: gateway.hardwareId })}
      description={t("platform.gateways.repair.description")}
      audit={t("platform.gateways.repair.audit")}
      confirmLabel={t("platform.gateways.repair.confirm")}
      pendingLabel={t("platform.dialog.working")}
      tone="destructive"
      confirmDisabled={!confirm.ready || note.trim() === ""}
      returnFocus={props.returnFocus}
      run={() => api.sendForRepair(gateway.hardwareId, { note: note.trim() })}
      onDone={() => props.onDone(t("platform.gateways.repair.done", { id: gateway.hardwareId }))}
    >
      <FormField label={t("platform.gateways.repair.note")} description={t("platform.gateways.repair.note_hint")}>
        {(control) => (
          <textarea
            {...control}
            rows={2}
            maxLength={500}
            value={note}
            className={cn(controlClasses, "h-auto min-h-16 py-2")}
            onChange={(event) => setNote(event.target.value)}
          />
        )}
      </FormField>
      <ConfirmByTyping
        label={t("platform.gateways.type_last4", { value: lastFour(gateway.hardwareId) })}
        expected={lastFour(gateway.hardwareId)}
        value={confirm.typed}
        onChange={confirm.setTyped}
      />
    </ActionDialog>
  );
}

const REASONS: readonly RevocationReason[] = [
  "unspecified",
  "key_compromise",
  "affiliation_changed",
  "superseded",
  "cessation_of_operation",
  "privilege_withdrawn",
];

const REASON_TEXT = {
  unspecified: "platform.gateways.revoke.reason.unspecified",
  key_compromise: "platform.gateways.revoke.reason.key_compromise",
  affiliation_changed: "platform.gateways.revoke.reason.affiliation_changed",
  superseded: "platform.gateways.revoke.reason.superseded",
  cessation_of_operation: "platform.gateways.revoke.reason.cessation_of_operation",
  privilege_withdrawn: "platform.gateways.revoke.reason.privilege_withdrawn",
} as const;

function RevokeDialog(props: Common) {
  const { t } = useI18n();
  const api = usePlatformApi();
  const { gateway } = props;
  const [reason, setReason] = useState<RevocationReason>("unspecified");
  const [typed, setTyped] = useState("");
  const reasonId = useId();
  return (
    <ActionDialog
      open
      onOpenChange={props.onOpenChange}
      title={t("platform.gateways.revoke.title", { id: gateway.hardwareId })}
      audit={t("platform.gateways.revoke.audit")}
      confirmLabel={t("platform.gateways.revoke.confirm")}
      pendingLabel={t("platform.dialog.working")}
      tone="destructive"
      confirmDisabled={!typedMatches(typed, gateway.hardwareId)}
      returnFocus={props.returnFocus}
      run={() => api.revokeGatewayCertificates(gateway.hardwareId, { reason })}
      onDone={() => props.onDone(t("platform.gateways.revoke.done", { id: gateway.hardwareId }))}
    >
      <div className="grid gap-2">
        <label htmlFor={reasonId} className="text-sm font-medium">
          {t("platform.gateways.revoke.reason")}
        </label>
        <NativeSelect
          id={reasonId}
          value={reason}
          onChange={(event) => setReason(event.target.value as RevocationReason)}
        >
          {REASONS.map((value) => (
            <option key={value} value={value}>
              {t(REASON_TEXT[value])}
            </option>
          ))}
        </NativeSelect>
        {reason === "key_compromise" ? (
          <p className="text-sm font-medium text-destructive">{t("platform.gateways.revoke.key_compromise")}</p>
        ) : null}
      </div>
      <ConfirmByTyping
        label={t("platform.gateways.type_full", { value: gateway.hardwareId })}
        expected={gateway.hardwareId}
        value={typed}
        onChange={setTyped}
      />
    </ActionDialog>
  );
}

function DecommissionDialog(props: Common) {
  const { t } = useI18n();
  const api = usePlatformApi();
  const { gateway } = props;
  const [typed, setTyped] = useState("");
  const [understood, setUnderstood] = useState(false);
  const checkId = useId();
  return (
    <ActionDialog
      open
      onOpenChange={props.onOpenChange}
      title={t("platform.gateways.decommission.title", { id: gateway.hardwareId })}
      description={t("platform.gateways.decommission.description")}
      audit={t("platform.gateways.decommission.audit")}
      confirmLabel={t("platform.gateways.decommission.confirm")}
      pendingLabel={t("platform.dialog.working")}
      tone="destructive"
      confirmDisabled={!typedMatches(typed, gateway.hardwareId) || !understood}
      returnFocus={props.returnFocus}
      run={() => api.decommissionGateway(gateway.hardwareId)}
      onDone={() => props.onDone(t("platform.gateways.decommission.done", { id: gateway.hardwareId }))}
    >
      <Alert variant="destructive" className="grid-cols-[1rem_1fr]">
        <TriangleAlertIcon aria-hidden="true" />
        <AlertTitle>{t("platform.gateways.decommission.irreversible")}</AlertTitle>
        <AlertDescription>
          <p>
            {gateway.tenant
              ? t("platform.gateways.decommission.bound", { tenant: gateway.tenant.name })
              : t("platform.gateways.decommission.unbound")}
          </p>
        </AlertDescription>
      </Alert>
      <ConfirmByTyping
        label={t("platform.gateways.type_full", { value: gateway.hardwareId })}
        expected={gateway.hardwareId}
        value={typed}
        onChange={setTyped}
      />
      <div className="flex items-center gap-2 text-sm">
        <input
          id={checkId}
          type="checkbox"
          checked={understood}
          onChange={(event) => setUnderstood(event.target.checked)}
          className="size-4 accent-primary"
        />
        <label htmlFor={checkId}>{t("platform.gateways.decommission.understood")}</label>
      </div>
    </ActionDialog>
  );
}
