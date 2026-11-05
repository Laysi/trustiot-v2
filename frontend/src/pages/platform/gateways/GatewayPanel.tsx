import { useInfiniteQuery } from "@tanstack/react-query";
import { PackageOpenIcon, SearchIcon, SearchXIcon, TriangleAlertIcon } from "lucide-react";
import { type ReactNode, useId, useRef, useState } from "react";
import { Link } from "react-router";
import { classifyError } from "../../../api/errors.ts";
import { usePlatformApi } from "../../../api/pending/PlatformApiProvider.tsx";
import type { GatewayRow } from "../../../api/pending/platform.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { controlClasses } from "../../../components/ui/input.tsx";
import { NativeSelect } from "../../../components/ui/native-select.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { PLATFORM_PROVISIONING_PATH } from "../paths.ts";
import { gatewaysKey } from "../queries.ts";
import { Skeleton } from "../ui/skeleton.tsx";
import { StatTile } from "../ui/stat-tile.tsx";
import { GatewayDialogs, type GatewayDialogTarget } from "./GatewayDialogs.tsx";
import { type GatewayAction, GatewayTable } from "./GatewayTable.tsx";

/** The gateways' health is read again this often, as the page says. */
const REFRESH_MS = 30_000;

type BindingFilter = "all" | "pool" | "bound" | "repair";
type HealthFilter = "all" | "online" | "degraded" | "offline";
const BINDING_FILTERS: readonly BindingFilter[] = ["all", "pool", "bound", "repair"];
const HEALTH_FILTERS: readonly HealthFilter[] = ["all", "online", "degraded", "offline"];
const BINDING_LABELS = {
  all: "platform.gateways.filter.all",
  pool: "platform.gateways.filter.pool",
  bound: "platform.gateways.filter.bound",
  repair: "platform.gateways.filter.repair",
} as const;
const HEALTH_LABELS = {
  all: "platform.gateways.filter.all",
  online: "platform.gateways.health.online",
  degraded: "platform.gateways.health.degraded",
  offline: "platform.gateways.health.offline",
} as const;

/**
 * The gateways of the whole platform, or of one tenant (`tenantId`): the health summary (the tiles
 * set the health filter), the filters, the table with its row actions and their dialogs
 * (docs/design/platform-admin/gateways.html). The health is read again every 30 seconds. When it
 * cannot be read the rows still say where each gateway is and the health reads "unknown", never
 * "offline". The assets and certs endpoints refuse a platform administrator whose current tenant is
 * suspended (403 `tenant_suspended`); the panel then says so, which is not an outage.
 */
export function GatewayPanel({ tenantId }: { tenantId?: string }) {
  const { t, errorMessage, formatDateTime } = useI18n();
  const api = usePlatformApi();
  const whole = tenantId === undefined;
  const [query, setQuery] = useState("");
  const [binding, setBinding] = useState<BindingFilter>("all");
  const [health, setHealth] = useState<HealthFilter>("all");
  const [target, setTarget] = useState<GatewayDialogTarget | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const trigger = useRef<HTMLElement | null>(null);
  const searchId = useId();
  const bindingId = useId();
  const healthId = useId();

  const list = useInfiniteQuery({
    queryKey: gatewaysKey(tenantId ?? ""),
    queryFn: ({ pageParam, signal }) =>
      api.listGateways(
        { ...(tenantId ? { tenantId } : {}), ...(pageParam !== undefined ? { cursor: pageParam } : {}) },
        signal,
      ),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.nextCursor ?? undefined,
    refetchInterval: REFRESH_MS,
  });

  const pages = list.data?.pages ?? [];
  const latest = pages[pages.length - 1];
  const first = pages[0];
  const loaded = pages.flatMap((page) => page.gateways);
  const needle = query.trim().toLowerCase();
  const shown = loaded
    .filter((gateway) => binding === "all" || gateway.placement === binding)
    .filter((gateway) => health === "all" || gateway.health === health)
    .filter(
      (gateway) =>
        needle === "" ||
        gateway.hardwareId.toLowerCase().includes(needle) ||
        (gateway.tenant?.name.toLowerCase().includes(needle) ?? false),
    );
  // The server filters nothing yet, so search and filters see only the pages loaded so far.
  const filtering = needle !== "" || binding !== "all" || health !== "all";
  const clear = () => {
    setQuery("");
    setBinding("all");
    setHealth("all");
  };

  const act = (action: GatewayAction, gateway: GatewayRow, element: HTMLElement | null) => {
    trigger.current = element;
    setMessage(null);
    setTarget({ action, gateway });
  };

  let body: ReactNode;
  if (list.isPending) {
    body = (
      <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5">
        <p role="status" className="sr-only">
          {t("platform.gateways.loading")}
        </p>
        {[0, 1, 2].map((row) => (
          <div key={row} className="flex items-center justify-between gap-4">
            <Skeleton className="w-1/5" />
            <Skeleton className="w-1/4" />
            <Skeleton className="w-1/6" />
          </div>
        ))}
      </section>
    );
  } else if (first === undefined) {
    const suspended = classifyError(list.error) === "tenant_suspended";
    body = suspended ? (
      <Alert role="status" className="grid-cols-[1rem_1fr] border-warning/50 bg-warning-soft text-foreground">
        <TriangleAlertIcon aria-hidden="true" />
        <AlertTitle>{t("platform.gateways.suspended_title")}</AlertTitle>
        <AlertDescription className="text-foreground">
          <p>{t("platform.gateways.suspended_body")}</p>
        </AlertDescription>
      </Alert>
    ) : (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p className="font-medium">{t("platform.gateways.load_failed")}</p>
          <p>{errorMessage(list.error)}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (loaded.length === 0) {
    body = (
      <div className="grid justify-items-center gap-3 rounded-lg border px-4 py-12 text-center">
        <PackageOpenIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <h2 className="font-semibold">
          {t(whole ? "platform.gateways.empty_title" : "platform.gateways.empty_tenant_title")}
        </h2>
        <p className="max-w-md text-sm text-muted-foreground">
          {t(whole ? "platform.gateways.empty_body" : "platform.gateways.empty_tenant_body")}
        </p>
        {whole ? (
          <Button asChild variant="outline">
            <Link to={PLATFORM_PROVISIONING_PATH}>{t("platform.gateways.empty_link")}</Link>
          </Button>
        ) : null}
      </div>
    );
  } else if (shown.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-3 rounded-lg border px-4 py-12 text-center">
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <h2 className="font-semibold">{t("platform.gateways.no_match_title")}</h2>
        <Button variant="outline" onClick={clear}>
          {t("platform.tenants.clear")}
        </Button>
      </div>
    );
  } else {
    body = <GatewayTable gateways={shown} showTenant={whole} onAction={act} />;
  }

  const summary = latest?.summary;
  return (
    <div className="grid gap-4">
      {whole && latest ? (
        <>
          <p className="text-xs text-muted-foreground">
            {t("platform.gateways.updated", { time: formatDateTime(latest.updatedAt) })}
          </p>
          {summary ? (
            <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
              <StatTile
                label={t("platform.gateways.tile.all")}
                value={summary.total}
                note={t("platform.gateways.tile.bound", { count: summary.bound })}
                pressed={health === "all" && binding === "all"}
                onClick={clear}
              />
              <StatTile
                label={t("platform.gateways.health.online")}
                value={summary.online}
                pressed={health === "online"}
                onClick={() => setHealth("online")}
              />
              <StatTile
                label={t("platform.gateways.health.degraded")}
                value={summary.degraded}
                note={t("platform.gateways.tile.degraded_note")}
                pressed={health === "degraded"}
                onClick={() => setHealth("degraded")}
              />
              <StatTile
                label={t("platform.gateways.health.offline")}
                value={summary.offline}
                note={t("platform.gateways.tile.offline_note", { count: summary.pool })}
                pressed={health === "offline"}
                onClick={() => setHealth("offline")}
              />
            </div>
          ) : null}
        </>
      ) : null}

      {latest && !latest.healthAvailable ? (
        <Alert
          role="status"
          className="grid-cols-[1fr_auto] items-center gap-x-3 border-warning/50 bg-warning-soft text-foreground"
        >
          <AlertDescription className="col-start-1 text-foreground">
            <p className="font-medium">{t("platform.gateways.health_unavailable_title")}</p>
            <p>{t("platform.gateways.health_unavailable_body")}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      ) : null}

      {whole && first !== undefined ? (
        <search aria-label={t("platform.gateways.filters")} className="flex flex-wrap items-end gap-3">
          <div className="relative grid min-w-56 flex-[1_1_16rem] gap-1.5">
            <label htmlFor={searchId} className="text-sm font-medium">
              {t("platform.gateways.search")}
            </label>
            <SearchIcon
              aria-hidden="true"
              className="pointer-events-none absolute bottom-2.5 left-3 size-4 text-muted-foreground"
            />
            <input
              id={searchId}
              type="search"
              value={query}
              className={cn(controlClasses, "pl-9")}
              onChange={(event) => setQuery(event.target.value)}
            />
          </div>
          <div className="grid w-48 gap-1.5">
            <label htmlFor={bindingId} className="text-sm font-medium">
              {t("platform.gateways.binding")}
            </label>
            <NativeSelect
              id={bindingId}
              value={binding}
              onChange={(event) => setBinding(event.target.value as BindingFilter)}
            >
              {BINDING_FILTERS.map((value) => (
                <option key={value} value={value}>
                  {t(BINDING_LABELS[value])}
                </option>
              ))}
            </NativeSelect>
          </div>
          <div className="grid w-40 gap-1.5">
            <label htmlFor={healthId} className="text-sm font-medium">
              {t("platform.gateways.health_filter")}
            </label>
            <NativeSelect
              id={healthId}
              value={health}
              onChange={(event) => setHealth(event.target.value as HealthFilter)}
            >
              {HEALTH_FILTERS.map((value) => (
                <option key={value} value={value}>
                  {t(HEALTH_LABELS[value])}
                </option>
              ))}
            </NativeSelect>
          </div>
        </search>
      ) : null}

      {filtering && list.hasNextPage ? (
        <p role="status" className="text-sm text-muted-foreground">
          {t("platform.gateways.partial_scope", { count: loaded.length })}
        </p>
      ) : null}

      {body}

      <div className="flex flex-wrap items-center justify-between gap-3 text-xs text-muted-foreground">
        <p role="status" className="min-h-4 text-foreground">
          {message}
        </p>
        {list.hasNextPage ? (
          <Button variant="outline" onClick={() => void list.fetchNextPage()} disabled={list.isFetchingNextPage}>
            {list.isFetchingNextPage ? t("platform.audit.loading_more") : t("platform.audit.load_more")}
          </Button>
        ) : null}
      </div>

      <GatewayDialogs
        target={target}
        onClose={() => setTarget(null)}
        onDone={setMessage}
        returnFocus={() => trigger.current}
      />
    </div>
  );
}
