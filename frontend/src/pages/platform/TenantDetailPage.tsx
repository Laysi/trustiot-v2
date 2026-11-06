import { useQuery } from "@tanstack/react-query";
import { BanIcon, CheckIcon, CopyIcon, GaugeIcon, SearchXIcon } from "lucide-react";
import { type ReactNode, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { classifyError } from "../../api/errors.ts";
import { usePlatformApi } from "../../api/pending/PlatformApiProvider.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { AuditEvents } from "./audit/AuditEvents.tsx";
import { TenantDialogs, type TenantDialogTarget } from "./dialogs/TenantDialogs.tsx";
import { isFinal, PLAN_LABELS } from "./format.ts";
import { GatewayPanel } from "./gateways/GatewayPanel.tsx";
import { PLATFORM_TENANTS_PATH, platformAuditPath, platformTenantPath, TENANT_TABS, type TenantTab } from "./paths.ts";
import { tenantKey } from "./queries.ts";
import { TenantStatusBadge } from "./TenantStatusBadge.tsx";
import { TenantMembers } from "./tenant/TenantMembers.tsx";
import { type OverviewAction, TenantOverview } from "./tenant/TenantOverview.tsx";
import { Skeleton } from "./ui/skeleton.tsx";

const PROCESSING_REFRESH_MS = 5000;

const TAB_LABELS = {
  overview: "platform.tenant.tab.overview",
  members: "platform.tenant.tab.members",
  gateways: "platform.tenant.tab.gateways",
  audit: "platform.tenant.tab.audit",
} as const;

function tabOf(value: string | null): TenantTab {
  return TENANT_TABS.find((tab) => tab === value) ?? "overview";
}

/**
 * One tenant (`/platform/tenants/:tenantId`, docs/design/platform-admin/tenant-detail.html): the
 * state, which is a timeline while a suspension is processing (read again every 5 seconds until
 * certs confirms), the quota, and the tabs overview, members (member rescue), gateways and the
 * tenant's operation log. The actions are in the header: adjust the quota, and suspend; resume and
 * change the reason are on the state card. A tenant whose contract was terminated has none of them.
 */
export function TenantDetailPage() {
  const { t, errorMessage } = useI18n();
  const api = usePlatformApi();
  const { tenantId = "" } = useParams();
  const [params] = useSearchParams();
  const tab = tabOf(params.get("tab"));
  const [target, setTarget] = useState<TenantDialogTarget | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const trigger = useRef<HTMLElement | null>(null);

  const query = useQuery({
    queryKey: tenantKey(tenantId),
    queryFn: ({ signal }) => api.getTenant(tenantId, signal),
    refetchInterval: (state) => (state.state.data?.status === "processing" ? PROCESSING_REFRESH_MS : false),
  });
  const tenant = query.data;

  const crumbs = [{ label: t("platform.tenants.title"), to: PLATFORM_TENANTS_PATH }];
  if (query.isPending) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={t("platform.tenant.loading_title")} crumbs={crumbs} />
        <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5">
          <p role="status" className="sr-only">
            {t("platform.tenant.loading")}
          </p>
          {[0, 1, 2].map((row) => (
            <Skeleton key={row} className="w-1/2" />
          ))}
        </section>
      </div>
    );
  }
  if (tenant === undefined) {
    const missing = classifyError(query.error) === "not_found";
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={t("platform.tenants.title")} crumbs={[]} />
        {missing ? (
          <div role="status" className="grid justify-items-center gap-3 rounded-lg border px-4 py-12 text-center">
            <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
            <h2 className="font-semibold">{t("platform.tenant.not_found_title")}</h2>
            <p className="text-sm text-muted-foreground">{t("platform.tenant.not_found_body")}</p>
            <Button asChild variant="outline">
              <Link to={PLATFORM_TENANTS_PATH}>{t("platform.tenant.back")}</Link>
            </Button>
          </div>
        ) : (
          <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
            <AlertDescription className="col-start-1">
              <p className="font-medium">{t("platform.tenant.load_failed")}</p>
              <p>{errorMessage(query.error)}</p>
            </AlertDescription>
            <Button variant="outline" onClick={() => void query.refetch()} disabled={query.isFetching}>
              {t("page.action.retry")}
            </Button>
          </Alert>
        )}
      </div>
    );
  }

  const final = isFinal(tenant);
  const open = (kind: TenantDialogTarget["kind"], element: HTMLElement | null) => {
    trigger.current = element;
    setMessage(null);
    setTarget({ kind, tenant });
  };
  const overview = (action: OverviewAction, element: HTMLElement | null) => open(action, element);

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={tenant.name}
        crumbs={crumbs}
        tags={<TenantStatusBadge status={tenant.status} reason={tenant.reason} />}
        meta={
          <span className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span>{t(PLAN_LABELS[tenant.plan])}</span>
            <span aria-hidden="true">·</span>
            <span className="font-mono text-xs wrap-anywhere">{tenant.id}</span>
            <CopyId id={tenant.id} />
          </span>
        }
        actions={
          final ? null : (
            <>
              <Button variant="outline" onClick={(event) => open("quota", event.currentTarget)}>
                <GaugeIcon />
                {t("platform.tenant.action.quota")}
              </Button>
              {tenant.status === "active" ? (
                <Button
                  variant="outline"
                  className="border-destructive text-destructive hover:text-destructive"
                  onClick={(event) => open("suspend", event.currentTarget)}
                >
                  <BanIcon />
                  {t("platform.tenant.action.suspend")}
                </Button>
              ) : null}
            </>
          )
        }
      />

      <nav aria-label={t("platform.tenant.tabs")} className="flex flex-wrap gap-1 border-b">
        {TENANT_TABS.map((value) => (
          <Link
            key={value}
            to={platformTenantPath(tenant.id, value)}
            replace
            aria-current={tab === value ? "page" : undefined}
            className={cn(
              "-mb-px rounded-t-md border-b-2 border-transparent px-3 py-2 text-sm hover:bg-accent",
              tab === value && "border-primary font-medium",
              focusRing,
            )}
          >
            {t(TAB_LABELS[value])}
          </Link>
        ))}
      </nav>

      <TabBody tab={tab}>
        {tab === "overview" ? <TenantOverview tenant={tenant} onAction={overview} /> : null}
        {tab === "members" ? <TenantMembers tenant={tenant} onDone={setMessage} /> : null}
        {tab === "gateways" ? <GatewayPanel tenantId={tenant.id} /> : null}
        {tab === "audit" ? (
          <div className="grid gap-3">
            <p className="text-sm text-muted-foreground">
              {t("platform.tenant.audit_note")}{" "}
              <Link className="underline underline-offset-4" to={platformAuditPath(tenant.id)}>
                {t("platform.tenant.audit_full")}
              </Link>
            </p>
            <AuditEvents scope={{ kind: "tenant", tenantId: tenant.id }} />
          </div>
        ) : null}
      </TabBody>

      <p role="status" className="min-h-4 text-sm">
        {message}
      </p>

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

function TabBody({ children }: { tab: TenantTab; children: ReactNode }) {
  return <div className="grid gap-4">{children}</div>;
}

/** Copies the tenant ID, and says so; clipboards are refused in some contexts, which it says too. */
function CopyId({ id }: { id: string }) {
  const { t } = useI18n();
  const [state, setState] = useState<"idle" | "done" | "failed">("idle");
  return (
    <>
      <Button
        variant="ghost"
        size="sm"
        className="h-7 px-2"
        aria-label={t("platform.tenant.copy_id")}
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(id);
            setState("done");
          } catch {
            setState("failed");
          }
        }}
      >
        {state === "done" ? <CheckIcon /> : <CopyIcon />}
        {t("platform.tenant.copy")}
      </Button>
      <span role="status" className="sr-only">
        {state === "done" ? t("platform.tenant.copied") : state === "failed" ? t("platform.tenant.copy_failed") : ""}
      </span>
    </>
  );
}
