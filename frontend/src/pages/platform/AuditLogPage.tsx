import { useQuery } from "@tanstack/react-query";
import { DownloadIcon, InfoIcon } from "lucide-react";
import { useId, useState } from "react";
import { useSearchParams } from "react-router";

import { usePlatformApi } from "../../api/pending/PlatformApiProvider.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { AuditEvents, EVENT_KINDS, type EventKind, type LoadedEvents } from "./audit/AuditEvents.tsx";
import { tenantsKey } from "./queries.ts";

const KIND_LABELS = {
  all: "platform.audit.kind.all",
  state: "platform.audit.kind.state",
  members: "platform.audit.kind.members",
  quota: "platform.audit.kind.quota",
  gateway: "platform.audit.kind.gateway",
} as const;

/** A `datetime-local` value as the instant it names in the browser's time zone, or `undefined`. */
function instant(value: string): string | undefined {
  if (value === "") return undefined;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? undefined : date.toISOString();
}

/**
 * The operation log (`/platform/audit-log`, checklist K2, docs/design/platform-admin/audit-log.html):
 * the platform-wide events (model roles, factory registrations) or one tenant's, chosen with a
 * segmented control and, for a tenant, a select (`?tenant=<ID>` links straight to it). A time range
 * (`from` before `to`, checked here so the server never sees a bad one), a type filter that works on
 * the loaded page only (decision Q13), and an export of the loaded events as JSON.
 */
export function AuditLogPage() {
  const { t } = useI18n();
  const api = usePlatformApi();
  const [params, setParams] = useSearchParams();
  const tenantParam = params.get("tenant");
  const scope: "platform" | "tenant" = tenantParam === null ? "platform" : "tenant";
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [kind, setKind] = useState<EventKind>("all");
  // The events on screen and the query they belong to: a page of another query is never exported.
  const [loaded, setLoaded] = useState<{ key: string; events: LoadedEvents }>({ key: "", events: [] });
  const fromId = useId();
  const toId = useId();
  const tenantId = useId();
  const kindId = useId();

  const tenants = useQuery({
    queryKey: tenantsKey,
    queryFn: ({ signal }) => api.listTenants(signal),
  });
  const options = tenants.data ?? [];
  const selected = tenantParam !== null && tenantParam !== "" ? tenantParam : (options[0]?.id ?? "");

  const fromInstant = instant(from);
  const toInstant = instant(to);
  const rangeInvalid = fromInstant !== undefined && toInstant !== undefined && fromInstant >= toInstant;

  const chooseScope = (next: "platform" | "tenant") => {
    setParams(
      (current) => {
        const params = new URLSearchParams(current);
        if (next === "platform") params.delete("tenant");
        else params.set("tenant", selected);
        return params;
      },
      { replace: true },
    );
  };

  const waitingForTenant = scope === "tenant" && selected === "";
  const queryKey =
    rangeInvalid || waitingForTenant ? null : `${scope}|${selected}|${fromInstant ?? ""}|${toInstant ?? ""}`;
  const exportable = queryKey !== null && loaded.key === queryKey ? loaded.events : [];

  const exportPage = () => {
    const blob = new Blob([JSON.stringify(exportable, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = "operation-log.json";
    link.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={t("platform.audit.title")}
        meta={t("platform.audit.description")}
        actions={
          <Button variant="outline" onClick={exportPage} disabled={exportable.length === 0}>
            <DownloadIcon />
            {t("platform.audit.export")}
          </Button>
        }
      />

      <search aria-label={t("platform.audit.filters")} className="flex flex-wrap items-end gap-3">
        <fieldset className="grid gap-1.5">
          <legend className="text-sm font-medium">{t("platform.audit.scope")}</legend>
          <div className="inline-flex gap-0.5 rounded-md bg-muted p-[3px]">
            {(["platform", "tenant"] as const).map((value) => (
              <Button
                key={value}
                size="sm"
                variant="ghost"
                aria-pressed={scope === value}
                className={cn(
                  "h-[30px] px-3 text-muted-foreground hover:bg-transparent",
                  scope === value && "bg-background text-foreground shadow-sm hover:bg-background",
                )}
                onClick={() => chooseScope(value)}
              >
                {t(value === "platform" ? "platform.audit.scope_platform" : "platform.audit.scope_tenant")}
              </Button>
            ))}
          </div>
        </fieldset>
        {scope === "tenant" ? (
          <div className="grid w-56 gap-1.5">
            <label htmlFor={tenantId} className="text-sm font-medium">
              {t("platform.audit.tenant")}
            </label>
            <NativeSelect
              id={tenantId}
              value={selected}
              onChange={(event) => setParams({ tenant: event.target.value }, { replace: true })}
            >
              {options.map((tenant) => (
                <option key={tenant.id} value={tenant.id}>
                  {tenant.name}
                </option>
              ))}
              {options.some((tenant) => tenant.id === selected) || selected === "" ? null : (
                <option value={selected}>{selected}</option>
              )}
            </NativeSelect>
          </div>
        ) : null}
        <div className="grid gap-1.5">
          <label htmlFor={fromId} className="text-sm font-medium">
            {t("platform.audit.from")}
          </label>
          <input
            id={fromId}
            type="datetime-local"
            value={from}
            aria-invalid={rangeInvalid ? true : undefined}
            className={cn(controlClasses, "w-48")}
            onChange={(event) => setFrom(event.target.value)}
          />
        </div>
        <div className="grid gap-1.5">
          <label htmlFor={toId} className="text-sm font-medium">
            {t("platform.audit.to")}
          </label>
          <input
            id={toId}
            type="datetime-local"
            value={to}
            aria-invalid={rangeInvalid ? true : undefined}
            className={cn(controlClasses, "w-48")}
            onChange={(event) => setTo(event.target.value)}
          />
        </div>
        <div className="grid w-48 gap-1.5">
          <label htmlFor={kindId} className="text-sm font-medium">
            {t("platform.audit.kind")}
          </label>
          <NativeSelect id={kindId} value={kind} onChange={(event) => setKind(event.target.value as EventKind)}>
            {EVENT_KINDS.map((value) => (
              <option key={value} value={value}>
                {t(KIND_LABELS[value])}
              </option>
            ))}
          </NativeSelect>
        </div>
      </search>

      {rangeInvalid ? (
        <p role="alert" className="text-sm font-medium text-destructive">
          {t("platform.audit.range_invalid")}
        </p>
      ) : null}

      {scope === "platform" ? (
        <Alert role="note" className="grid-cols-[1rem_1fr]">
          <InfoIcon aria-hidden="true" />
          <AlertTitle>{t("platform.audit.platform_note_title")}</AlertTitle>
          <AlertDescription>
            <p>{t("platform.audit.platform_note_body")}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      {queryKey === null ? null : (
        <AuditEvents
          scope={scope === "platform" ? { kind: "platform" } : { kind: "tenant", tenantId: selected }}
          {...(fromInstant ? { from: fromInstant } : {})}
          {...(toInstant ? { to: toInstant } : {})}
          kind={kind}
          onLoaded={(events) => setLoaded({ key: queryKey, events })}
        />
      )}
    </div>
  );
}
