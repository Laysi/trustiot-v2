import { useQuery } from "@tanstack/react-query";
import {
  BanIcon,
  BuildingIcon,
  EllipsisIcon,
  ExternalLinkIcon,
  GaugeIcon,
  LockIcon,
  PlayIcon,
  PlusIcon,
  ScrollTextIcon,
  SearchIcon,
  SearchXIcon,
  TriangleAlertIcon,
  UsersIcon,
} from "lucide-react";
import { type ReactNode, useId, useRef, useState } from "react";
import { Link } from "react-router";

import { usePlatformApi } from "../../api/pending/PlatformApiProvider.tsx";
import type { Plan, TenantStatus, TenantSummary } from "../../api/pending/platform.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../../components/ui/dropdown-menu.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { TenantDialogs, type TenantDialogTarget } from "./dialogs/TenantDialogs.tsx";
import { formatUsd, isFinal, isFull, limitNumber, PLAN_LABELS, REASON_LABELS } from "./format.ts";
import { PLATFORM_TENANT_NEW_PATH, platformAuditPath, platformTenantPath } from "./paths.ts";
import { shortId, tenantsKey } from "./queries.ts";
import { TenantStatusBadge } from "./TenantStatusBadge.tsx";
import { Badge } from "./ui/badge.tsx";
import { Meter } from "./ui/meter.tsx";
import { Skeleton } from "./ui/skeleton.tsx";
import { StackTable, Td, Th, Tr } from "./ui/stack-table.tsx";
import { WaitedFor } from "./Waiting.tsx";

/** A processing suspension is read again this often, until certs has confirmed it. */
const PROCESSING_REFRESH_MS = 5000;

type StatusFilter = "all" | TenantStatus;
type PlanFilter = "all" | Plan;
const STATUS_FILTERS: readonly StatusFilter[] = ["all", "active", "processing", "suspended", "deleted"];
const PLAN_FILTERS: readonly PlanFilter[] = ["all", "starter", "business", "enterprise", "enterprise_on_prem"];

/**
 * The tenants (`/platform/tenants`, checklist K1, docs/design/platform-admin/tenants.html): every
 * tenant including the suspended and deleted ones, with plan, state, gateway and member counts and
 * the month's AI spend; a search by name or ID and filters by state and plan; each row's actions
 * by state (suspend for a normal tenant, resume for a suspended one, neither once the contract is
 * terminated). While a suspension is processing the list is read again every 5 seconds.
 */
export function TenantsPage() {
  const { t, errorMessage } = useI18n();
  const api = usePlatformApi();
  const { session } = useSession();
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<StatusFilter>("all");
  const [plan, setPlan] = useState<PlanFilter>("all");
  const [target, setTarget] = useState<TenantDialogTarget | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const trigger = useRef<HTMLElement | null>(null);
  const searchId = useId();
  const statusId = useId();
  const planId = useId();

  const list = useQuery({
    queryKey: tenantsKey,
    queryFn: ({ signal }) => api.listTenants(signal),
    refetchInterval: (state) =>
      state.state.data?.some((tenant) => tenant.status === "processing") ? PROCESSING_REFRESH_MS : false,
  });

  const needle = query.trim().toLowerCase();
  const all = list.data ?? [];
  const shown = all
    .filter((tenant) => status === "all" || tenant.status === status)
    .filter((tenant) => plan === "all" || tenant.plan === plan)
    .filter((tenant) => needle === "" || tenant.name.toLowerCase().includes(needle) || tenant.id.includes(needle));
  const clear = () => {
    setQuery("");
    setStatus("all");
    setPlan("all");
  };

  const open = (kind: TenantDialogTarget["kind"], tenant: TenantSummary, element: HTMLElement | null) => {
    trigger.current = element;
    setMessage(null);
    setTarget({ kind, tenant });
  };

  // The tenant the administrator works in is itself suspended: the services refuse it, so the
  // gateway pages cannot be used until it is resumed or another tenant is chosen.
  const currentSuspended =
    session?.activeTenant && session.activeTenant.state !== "active" ? session.activeTenant : null;
  const currentRecord = currentSuspended ? all.find((tenant) => tenant.id === currentSuspended.id) : undefined;

  let body: ReactNode;
  if (list.isPending) {
    body = <TenantsSkeleton label={t("platform.tenants.loading")} />;
  } else if (list.data === undefined) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p className="font-medium">{t("platform.tenants.load_failed")}</p>
          <p>{errorMessage(list.error)}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (all.length === 0) {
    body = (
      <div className="grid justify-items-center gap-3 rounded-lg border px-4 py-12 text-center">
        <BuildingIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <h2 className="font-semibold">{t("platform.tenants.empty_title")}</h2>
        <p className="max-w-md text-sm text-muted-foreground">{t("platform.tenants.empty_body")}</p>
        <Button asChild>
          <Link to={PLATFORM_TENANT_NEW_PATH}>
            <PlusIcon />
            {t("platform.tenants.create_first")}
          </Link>
        </Button>
      </div>
    );
  } else if (shown.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-3 rounded-lg border px-4 py-12 text-center">
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <h2 className="font-semibold">{t("platform.tenants.no_match_title")}</h2>
        <p className="text-sm text-muted-foreground">{t("platform.tenants.no_match_body")}</p>
        <Button variant="outline" onClick={clear}>
          {t("platform.tenants.clear")}
        </Button>
      </div>
    );
  } else {
    body = <TenantTable tenants={shown} onOpen={open} />;
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={t("platform.tenants.title")}
        meta={t("platform.tenants.description")}
        actions={
          <Button asChild>
            <Link to={PLATFORM_TENANT_NEW_PATH}>
              <PlusIcon />
              {t("platform.tenants.create")}
            </Link>
          </Button>
        }
      />

      {currentSuspended ? (
        <Alert role="status" className="grid-cols-[1rem_1fr] border-warning/50 bg-warning-soft text-foreground">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertTitle>
            {t("platform.tenants.current_suspended_title", { name: currentSuspended.name ?? "" })}
          </AlertTitle>
          <AlertDescription className="text-foreground">
            <p>{t("platform.tenants.current_suspended_body")}</p>
            {currentRecord && currentRecord.status !== "active" && !isFinal(currentRecord) ? (
              <Button
                variant="outline"
                size="sm"
                onClick={(event) => open("resume", currentRecord, event.currentTarget)}
              >
                {t("platform.tenants.resume_named", { name: currentRecord.name })}
              </Button>
            ) : null}
          </AlertDescription>
        </Alert>
      ) : null}

      <search aria-label={t("platform.tenants.filters")} className="flex flex-wrap items-end gap-3">
        <div className="relative grid min-w-56 flex-[1_1_16rem] gap-1.5">
          <label htmlFor={searchId} className="text-sm font-medium">
            {t("platform.tenants.search")}
          </label>
          <SearchIcon
            aria-hidden="true"
            className="pointer-events-none absolute bottom-2.5 left-3 size-4 text-muted-foreground"
          />
          <input
            id={searchId}
            type="search"
            value={query}
            placeholder={t("platform.tenants.search_placeholder")}
            className={cn(controlClasses, "pl-9")}
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
        <div className="grid w-44 gap-1.5">
          <label htmlFor={statusId} className="text-sm font-medium">
            {t("platform.tenants.status")}
          </label>
          <NativeSelect
            id={statusId}
            value={status}
            onChange={(event) => setStatus(event.target.value as StatusFilter)}
          >
            {STATUS_FILTERS.map((value) => (
              <option key={value} value={value}>
                {value === "all" ? t("platform.tenants.status_all", { count: all.length }) : t(STATUS_LABELS[value])}
              </option>
            ))}
          </NativeSelect>
        </div>
        <div className="grid w-40 gap-1.5">
          <label htmlFor={planId} className="text-sm font-medium">
            {t("platform.tenants.plan")}
          </label>
          <NativeSelect id={planId} value={plan} onChange={(event) => setPlan(event.target.value as PlanFilter)}>
            {PLAN_FILTERS.map((value) => (
              <option key={value} value={value}>
                {value === "all" ? t("platform.tenants.plan_all") : t(PLAN_LABELS[value])}
              </option>
            ))}
          </NativeSelect>
        </div>
      </search>

      {body}

      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 text-xs text-muted-foreground">
        <p role="status" className="min-h-4 text-foreground">
          {message}
        </p>
        {list.data !== undefined && list.data.length > 0 ? (
          <p>
            {t("platform.tenants.footer", { shown: shown.length, total: all.length })}
            {all.some((tenant) => tenant.status === "processing") ? ` ${t("platform.tenants.refreshing")}` : ""}
          </p>
        ) : null}
      </div>

      <TenantDialogs
        target={target}
        onClose={() => setTarget(null)}
        onDone={setMessage}
        onStale={() => setMessage(t("platform.stale.refreshed"))}
        returnFocus={() => trigger.current}
      />
    </div>
  );
}

const STATUS_LABELS = {
  active: "platform.status.active",
  processing: "platform.status.processing",
  suspended: "platform.status.suspended",
  deleted: "platform.status.deleted",
} as const;

function TenantsSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1, 2].map((row) => (
        <div key={row} className="flex items-center justify-between gap-4">
          <Skeleton className="w-2/5" />
          <Skeleton className="w-1/6" />
          <Skeleton className="w-1/6" />
          <Skeleton className="w-1/12" />
        </div>
      ))}
    </section>
  );
}

function TenantTable(props: {
  tenants: readonly TenantSummary[];
  onOpen(kind: TenantDialogTarget["kind"], tenant: TenantSummary, element: HTMLElement | null): void;
}) {
  const { t, locale } = useI18n();
  return (
    <StackTable
      caption={t("platform.tenants.caption", { count: props.tenants.length })}
      head={
        <>
          <Th>{t("platform.tenants.column.tenant")}</Th>
          <Th>{t("platform.tenants.column.plan")}</Th>
          <Th>{t("platform.tenants.column.status")}</Th>
          <Th>{t("platform.tenants.column.gateways")}</Th>
          <Th>{t("platform.tenants.column.members")}</Th>
          <Th>{t("platform.tenants.column.ai")}</Th>
          <Th>
            <span className="sr-only">{t("platform.tenants.column.actions")}</span>
          </Th>
        </>
      }
    >
      {props.tenants.map((tenant) => (
        <Tr key={tenant.id}>
          <Td label={t("platform.tenants.column.tenant")} className="sm:min-w-56">
            <span className="grid justify-items-start gap-0.5">
              <Link
                to={platformTenantPath(tenant.id)}
                className={cn("rounded-sm font-medium underline-offset-4 hover:underline", focusRing)}
              >
                {tenant.name}
              </Link>
              <span className="font-mono text-xs text-muted-foreground">{shortId(tenant.id)}</span>
            </span>
          </Td>
          <Td label={t("platform.tenants.column.plan")}>{t(PLAN_LABELS[tenant.plan])}</Td>
          <Td label={t("platform.tenants.column.status")}>
            <StatusCell tenant={tenant} />
          </Td>
          <Td label={t("platform.tenants.column.gateways")} className="sm:min-w-32">
            <span className="grid gap-1">
              <span className="tabular-nums">
                {tenant.gateways.used} /{" "}
                {tenant.gateways.limit === "unlimited" ? t("platform.limit.unlimited") : tenant.gateways.limit}
                {isFull(tenant.gateways.used, tenant.gateways.limit) && tenant.gateways.limit !== 0 ? (
                  <span className="ml-1 text-xs text-muted-foreground">{t("platform.full")}</span>
                ) : null}
              </span>
              {tenant.gateways.limit !== "unlimited" ? (
                <Meter
                  value={tenant.gateways.used}
                  max={limitNumber(tenant.gateways.limit)}
                  label={t("platform.tenants.gateways_meter", { name: tenant.name })}
                />
              ) : null}
            </span>
          </Td>
          <Td label={t("platform.tenants.column.members")} className="tabular-nums">
            {tenant.members.used}
          </Td>
          <Td label={t("platform.tenants.column.ai")}>
            <span className="grid justify-items-start gap-1">
              <span className="tabular-nums">
                {formatUsd(tenant.ai.usedUsd, locale)} / {formatUsd(tenant.ai.limitUsd, locale)}
              </span>
              {tenant.ai.usedUsd > tenant.ai.limitUsd ? (
                <Badge tone="danger">
                  <TriangleAlertIcon aria-hidden="true" />
                  {t("platform.tenants.overdrawn")}
                </Badge>
              ) : null}
            </span>
          </Td>
          <Td label={t("platform.tenants.column.actions")} className="sm:text-right">
            <RowMenu tenant={tenant} onOpen={props.onOpen} />
          </Td>
        </Tr>
      ))}
    </StackTable>
  );
}

function StatusCell({ tenant }: { tenant: TenantSummary }) {
  const { t } = useI18n();
  return (
    <span className="grid justify-items-start gap-1">
      <TenantStatusBadge status={tenant.status} reason={tenant.reason} />
      {tenant.status === "processing" ? (
        <span className="text-xs text-muted-foreground">
          {t("platform.tenants.processing_note")} <WaitedFor since={tenant.stateChangedAt} />
        </span>
      ) : tenant.reason ? (
        <span className="text-xs text-muted-foreground">
          {tenant.reason === "contract_terminated" ? t("platform.tenants.final_note") : t(REASON_LABELS[tenant.reason])}
        </span>
      ) : null}
    </span>
  );
}

/** The row's actions by state: suspend, or resume, or neither once the contract was terminated. */
function RowMenu(props: {
  tenant: TenantSummary;
  onOpen(kind: TenantDialogTarget["kind"], tenant: TenantSummary, element: HTMLElement | null): void;
}) {
  const { t } = useI18n();
  const { tenant } = props;
  const final = isFinal(tenant);
  const button = useRef<HTMLButtonElement>(null);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          ref={button}
          variant="ghost"
          size="icon"
          aria-label={t("platform.tenants.actions_of", { name: tenant.name })}
        >
          <EllipsisIcon />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-56">
        <DropdownMenuItem asChild>
          <Link to={platformTenantPath(tenant.id)}>
            <ExternalLinkIcon />
            {t("platform.tenants.menu.open")}
          </Link>
        </DropdownMenuItem>
        {final ? null : (
          <DropdownMenuItem onSelect={() => props.onOpen("quota", tenant, button.current)}>
            <GaugeIcon />
            {t("platform.tenants.menu.quota")}
          </DropdownMenuItem>
        )}
        <DropdownMenuItem asChild>
          <Link to={platformTenantPath(tenant.id, "members")}>
            <UsersIcon />
            {t("platform.tenants.menu.members")}
          </Link>
        </DropdownMenuItem>
        <DropdownMenuItem asChild>
          <Link to={platformAuditPath(tenant.id)}>
            <ScrollTextIcon />
            {t("platform.tenants.menu.audit")}
          </Link>
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        {final ? (
          <DropdownMenuItem disabled>
            <LockIcon />
            {t("platform.tenants.menu.final")}
          </DropdownMenuItem>
        ) : tenant.status === "active" ? (
          <DropdownMenuItem variant="destructive" onSelect={() => props.onOpen("suspend", tenant, button.current)}>
            <BanIcon />
            {t("platform.tenants.menu.suspend")}
          </DropdownMenuItem>
        ) : (
          <DropdownMenuItem onSelect={() => props.onOpen("resume", tenant, button.current)}>
            <PlayIcon />
            {t("platform.tenants.menu.resume")}
          </DropdownMenuItem>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
