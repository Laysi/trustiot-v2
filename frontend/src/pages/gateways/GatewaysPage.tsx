import { useQuery, useQueryClient } from "@tanstack/react-query";
import { CheckIcon, PlusIcon, RefreshCwIcon, RouterIcon, SearchXIcon } from "lucide-react";
import { type ReactNode, useEffect, useId, useMemo, useRef, useState } from "react";
import { Link, useLocation, useSearchParams } from "react-router";

import type { Gateway } from "../../api/gateways.ts";
import { useGatewaysApi } from "../../api/pending/GatewaysApiProvider.tsx";
import { GATEWAYS_PATH } from "../../app/paths.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { BindDialog } from "./BindDialog.tsx";
import { useGatewayFormat } from "./format.ts";
import { GatewayList, type GatewayRow } from "./GatewayList.tsx";
import { StateMark } from "./PresenceBadge.tsx";
import { gatewayAllowanceKey, gatewayHealthKey, gatewaysKey } from "./queries.ts";
import { GatewayQuotaNotice } from "./UpgradeNotice.tsx";

/** How often the list reads the gateways again (while the window is in the foreground). */
export const REFRESH_MS = 30_000;

/** How long the gateway a link named, or one just bound, keeps its highlight. */
const HIGHLIGHT_MS = 3000;

/** `?status=` filters by whether a gateway is connected; the filter lives in the address, so a link keeps it. */
const STATUS_PARAM = "status";
const BIND_PARAM = "bind";
type StatusFilter = "online" | "offline" | "pending";

function statusFilter(value: string | null): StatusFilter | null {
  return value === "online" || value === "offline" || value === "pending" ? value : null;
}

function matches(gateway: Gateway, filter: StatusFilter): boolean {
  return filter === "pending" ? gateway.presence === "unknown" : gateway.presence === filter;
}

/** The hardware ID a link's `#fragment` names, or `null`. */
function hashTarget(hash: string): string | null {
  if (hash.length <= 1) return null;
  try {
    return decodeURIComponent(hash.slice(1));
  } catch {
    return null;
  }
}

/**
 * The tenant's gateways (`/gateways`, checklist C3 and D3; docs/design/tenant-admin/ design.md,
 * 3.3): whether each is connected, its last report, the configuration it runs, its devices'
 * health and its operating metrics, and for administrators the self-service binding. Every role
 * sees the list; only an administrator has the bind button (decision 2). The list reads again
 * every 30 seconds without a skeleton, and when a read fails with data on the screen it keeps the
 * data and says when it is from. Binding and health are pending APIs (api/pending/gateways.ts),
 * so the page is only in the UI preview until they exist.
 */
export function GatewaysPage() {
  // Keyed by the tenant, so what the page remembers (the gateway just bound, the open rows) is
  // dropped when the session moves to another tenant in place, never shown against its gateways.
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  return <GatewaysContent key={tenantId} />;
}

function GatewaysContent() {
  const { t, errorMessage } = useI18n();
  const format = useGatewayFormat();
  const api = useGatewaysApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const admin = tenant?.role === "admin";
  const [params, setParams] = useSearchParams();
  const location = useLocation();
  const quotaId = useId();
  const bindButton = useRef<HTMLButtonElement>(null);

  const gateways = useQuery({
    queryKey: gatewaysKey(tenantId),
    queryFn: ({ signal }) => api.listGateways(signal),
    refetchInterval: REFRESH_MS,
  });
  const health = useQuery({
    queryKey: gatewayHealthKey(tenantId),
    queryFn: ({ signal }) => api.listGatewayHealth(signal),
    refetchInterval: REFRESH_MS,
  });
  const allowance = useQuery({
    queryKey: gatewayAllowanceKey(tenantId),
    queryFn: ({ signal }) => api.getGatewayAllowance(signal),
  });

  const filter = statusFilter(params.get(STATUS_PARAM));
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const [highlight, setHighlight] = useState<string | null>(null);
  const [focusTarget, setFocusTarget] = useState<string | null>(null);
  const [bound, setBound] = useState<string | null>(null);

  const loaded = gateways.data;
  const target = hashTarget(location.hash);
  const targetKnown = target !== null && (loaded?.some((gateway) => gateway.hardware_id === target) ?? false);
  // A link to `#<hardware ID>` opens that gateway, scrolls to it and highlights it for a moment.
  useEffect(() => {
    if (!targetKnown || target === null) return;
    setExpanded((current) => new Set(current).add(target));
    setFocusTarget(target);
    setHighlight(target);
    const timer = setTimeout(() => setHighlight(null), HIGHLIGHT_MS);
    return () => clearTimeout(timer);
  }, [targetKnown, target]);

  const healthById = useMemo(
    () => new Map((health.data ?? []).map((entry) => [entry.hardware_id, entry])),
    [health.data],
  );
  const all = loaded ?? [];
  const rows: GatewayRow[] = all
    .filter((gateway) => filter === null || matches(gateway, filter))
    // The gateway just bound comes first, so the administrator sees it appear.
    .sort((a, b) => Number(b.hardware_id === bound) - Number(a.hardware_id === bound))
    .map((gateway) => ({ gateway, health: healthById.get(gateway.hardware_id) }));
  const counts = {
    online: all.filter((gateway) => gateway.presence === "online").length,
    offline: all.filter((gateway) => gateway.presence === "offline").length,
    pending: all.filter((gateway) => gateway.presence === "unknown").length,
  };

  const limit = allowance.data?.maxGateways;
  const full = typeof limit === "number" && loaded !== undefined && loaded.length >= limit;
  // The form opens only once the list and the plan's limit have answered (successfully or not): a
  // link that opened it earlier would close it, input and all, when the limit turned out to be reached.
  const quotaSettled = !gateways.isPending && !allowance.isPending;
  const bindOpen = admin && quotaSettled && !full && params.has(BIND_PARAM);

  const setParam = (name: string, value: string | null) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value === null) next.delete(name);
        else next.set(name, value);
        return next;
      },
      { replace: true },
    );
  const openBind = () => setParam(BIND_PARAM, "");
  const refresh = () => {
    void gateways.refetch();
    void health.refetch();
  };
  const toggle = (hardwareId: string) =>
    setExpanded((current) => {
      const next = new Set(current);
      if (!next.delete(hardwareId)) next.add(hardwareId);
      return next;
    });

  const onBound = (gateway: Gateway) => {
    setBound(gateway.hardware_id);
    setHighlight(gateway.hardware_id);
    setTimeout(() => setHighlight((current) => (current === gateway.hardware_id ? null : current)), HIGHLIGHT_MS);
    setParam(BIND_PARAM, null);
    void queryClient.invalidateQueries({ queryKey: gatewaysKey(tenantId) });
    void queryClient.invalidateQueries({ queryKey: gatewayHealthKey(tenantId) });
    void queryClient.invalidateQueries({ queryKey: gatewayAllowanceKey(tenantId) });
  };

  const bindAction = admin ? (
    <Button ref={bindButton} onClick={openBind} disabled={full} aria-describedby={full ? quotaId : undefined}>
      <PlusIcon aria-hidden="true" />
      {t("gateways.bind.open")}
    </Button>
  ) : null;

  let body: ReactNode;
  if (gateways.isPending) {
    body = <ListSkeleton label={t("gateways.loading")} />;
  } else if (loaded === undefined) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("gateways.load_failed", { reason: errorMessage(gateways.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void gateways.refetch()} disabled={gateways.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (all.length === 0) {
    body = (
      <EmptyState
        icon={<RouterIcon aria-hidden="true" />}
        title={t("gateways.empty.title")}
        action={
          admin && !full ? (
            <Button onClick={openBind}>
              <PlusIcon aria-hidden="true" />
              {t("gateways.empty.action")}
            </Button>
          ) : undefined
        }
      >
        {admin ? (
          <>
            <p>{t("gateways.empty.body")}</p>
            <p>{t("gateways.empty.need")}</p>
          </>
        ) : (
          <p>{t("gateways.empty.viewer", { tenant: tenant?.name ?? "" })}</p>
        )}
      </EmptyState>
    );
  } else if (rows.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <p className="font-semibold">{t("gateways.no_match.title")}</p>
        <Button variant="outline" size="sm" onClick={() => setParam(STATUS_PARAM, null)}>
          {t("gateways.no_match.clear")}
        </Button>
      </div>
    );
  } else {
    body = (
      <GatewayList
        rows={rows}
        now={Date.now()}
        expanded={expanded}
        onToggle={toggle}
        highlighted={highlight}
        focusTarget={focusTarget}
      />
    );
  }

  const summary: { id: StatusFilter; tone: "online" | "offline" | "unknown"; text: string }[] = [
    { id: "online", tone: "online", text: t("gateways.summary.online", { count: counts.online }) },
    { id: "offline", tone: "offline", text: t("gateways.summary.offline", { count: counts.offline }) },
    { id: "pending", tone: "unknown", text: t("gateways.summary.pending", { count: counts.pending }) },
  ];

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={t("gateways.title")} meta={t("gateways.description")} actions={bindAction} />

      {full && typeof limit === "number" && allowance.data ? (
        <GatewayQuotaNotice id={quotaId} plan={allowance.data.plan} used={all.length} limit={limit} />
      ) : null}

      {bound !== null ? (
        <Alert role="status">
          <CheckIcon aria-hidden="true" />
          <AlertTitle>{t("gateways.bound.title", { gateway: bound })}</AlertTitle>
          <AlertDescription>
            <p>{t("gateways.bound.body")}</p>
            <Button variant="ghost" size="sm" className="mt-1" onClick={() => setBound(null)}>
              {t("gateways.bound.dismiss")}
            </Button>
          </AlertDescription>
        </Alert>
      ) : null}

      {loaded !== undefined && all.length > 0 ? (
        <nav aria-label={t("gateways.summary.label")} className="flex flex-wrap items-center gap-x-5 gap-y-2 text-sm">
          {summary.map((item) => {
            const active = filter === item.id;
            return (
              <Link
                key={item.id}
                to={active ? GATEWAYS_PATH : `${GATEWAYS_PATH}?${STATUS_PARAM}=${item.id}`}
                aria-current={active ? "true" : undefined}
                className={cn("rounded-sm px-1 underline-offset-4 hover:underline", active && "bg-accent", focusRing)}
              >
                <StateMark tone={item.tone} className="font-normal text-foreground">
                  {item.text}
                </StateMark>
              </Link>
            );
          })}
          <span className="ml-auto flex items-center gap-1 text-muted-foreground">
            {t("gateways.summary.auto")} ·
            <Button variant="ghost" size="sm" onClick={refresh} className="text-muted-foreground">
              <RefreshCwIcon aria-hidden="true" />
              {t("gateways.summary.refresh")}
            </Button>
          </span>
        </nav>
      ) : null}

      {gateways.isError && loaded !== undefined ? (
        <Alert role="status" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{t("gateways.stale", { time: format.clock(gateways.dataUpdatedAt) })}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void gateways.refetch()} disabled={gateways.isFetching}>
            {t("gateways.retry_now")}
          </Button>
        </Alert>
      ) : null}

      {body}

      {health.isError && loaded !== undefined ? (
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{t("gateways.health_failed", { reason: errorMessage(health.error) })}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void health.refetch()} disabled={health.isFetching}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      ) : null}

      {bindOpen ? (
        <BindDialog
          tenantName={tenant?.name ?? null}
          returnFocus={bindButton}
          onClose={() => setParam(BIND_PARAM, null)}
          onBound={onBound}
        />
      ) : null}
    </div>
  );
}

/** Rows that stand in for the list while it loads; announced once, not row by row. */
function ListSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5 sm:px-6">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1, 2, 3].map((row) => (
        <div key={row} aria-hidden="true" className="flex items-center justify-between gap-4">
          <span className="h-3 w-[16%] animate-pulse rounded bg-muted" />
          <span className="h-3 w-[16%] animate-pulse rounded bg-muted" />
          <span className="h-3 w-[12%] animate-pulse rounded bg-muted" />
          <span className="h-3 w-[22%] animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}
