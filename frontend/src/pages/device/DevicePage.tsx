import { type UseQueryResult, useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronLeftIcon, ChevronRightIcon, CpuIcon } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { Device } from "../../api/devices.ts";
import { classifyError } from "../../api/errors.ts";
import { useBackToConversation } from "../../app/back-to-conversation.ts";
import { DEVICES_PATH } from "../../app/paths.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantName } from "../../session/tenant-name.ts";
import { StatusPage } from "../StatusPage.tsx";
import { useAutoRefresh } from "./autoRefresh.ts";
import { DeviceHeader, StaleBanner } from "./DeviceHeader.tsx";
import { DeviceInfo, NoDataPoints, NoReadings } from "./DeviceInfo.tsx";
import { HistoryCard } from "./HistoryCard.tsx";
import { dataHealth } from "./health.ts";
import { LatestCard } from "./LatestCard.tsx";
import { chartablePoints, choosePoint } from "./points.ts";
import { deviceKey, isSeriesKey, latestKey, modelKey } from "./queries.ts";
import { parseRange, type RangeChoice, withRange } from "./time.ts";
import { CLOCK_TICK_MS, useNow } from "./useNow.ts";

/** The device IDs the API hands out: lowercase hyphenated UUIDs (`DeviceId` in api/openapi.json). */
const DEVICE_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * One device (`/devices/:deviceId`, checklist I3), as docs/design/device-page/ draws it: whether
 * its data is coming in, the newest stored value of each data point, and the history of one data
 * point on the dashboard's time-series chart; what only matters when something is wrong comes
 * last. The chart's data point and range live in the address bar (`point`, `range` or `from` and
 * `to`), so a link opens the same view. Live values, a health API and CSV export come with
 * Laysi/trustiot-v2#454.
 */
export function DevicePage() {
  const { deviceId = "" } = useParams();
  if (!DEVICE_ID.test(deviceId)) {
    return <DeviceNotFound />;
  }
  // A new device starts from a new page state: its own range anchor and selection.
  return <DeviceView key={deviceId.toLowerCase()} deviceId={deviceId.toLowerCase()} />;
}

function DeviceView({ deviceId }: { deviceId: string }) {
  const { t } = useI18n();
  const api = useDeviceApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const [params, setParams] = useSearchParams();
  // A preset range ends at this instant: set when the page opens, the range changes, or the
  // page refreshes (the user's or the timer's), never on a render (a moving end would refetch
  // forever). It is also the instant of the last refresh, which the timer counts from.
  const [anchor, setAnchor] = useState(() => Date.now());
  // The anchor an automatic refresh set: while it is the current one, what loads is a refresh
  // nobody asked for, so it shows no spinner, disabled button or "updating" text.
  const [automaticAnchor, setAutomaticAnchor] = useState<number | null>(null);
  const quiet = automaticAnchor === anchor;
  // Whether the chart is zoomed in: a new series would put it back to the full range.
  const [zoomed, setZoomed] = useState(false);
  const [historyFetching, setHistoryFetching] = useState(false);
  const tick = useNow(CLOCK_TICK_MS);

  const device = useQuery({
    queryKey: deviceKey(tenant, deviceId),
    queryFn: ({ signal }) => api.getDevice(deviceId, signal),
  });
  const modelId = device.data?.model_id;
  const model = useQuery({
    queryKey: modelKey(tenant, modelId ?? ""),
    queryFn: ({ signal }) => api.getDeviceModel(modelId ?? "", signal),
    enabled: modelId !== undefined,
  });
  const latest = useQuery({
    queryKey: latestKey(tenant, deviceId),
    queryFn: ({ signal }) => api.getLatest(deviceId, signal),
    enabled: device.isSuccess,
  });
  // The quiet of an automatic refresh ends when what it loads has loaded, so a later request (another
  // time zone, a retry) is seen as usual. Only the change from loading to done counts: the first
  // render after the refresh starts may not show the loading yet.
  const loading = device.isFetching || model.isFetching || latest.isFetching || historyFetching;
  const wasLoading = useRef(false);
  useEffect(() => {
    if (wasLoading.current && !loading) setAutomaticAnchor(null);
    wasLoading.current = loading;
  }, [loading]);

  const range = parseRange(params);
  const points = chartablePoints(model.data, latest.data);
  // Until the model answers, the chart waits: its points decide which one to show first.
  const keys = model.isPending ? [] : points;
  const point = choosePoint(params.get("point"), keys);

  const setRange = useCallback(
    (choice: RangeChoice) => {
      setAnchor(Date.now());
      setAutomaticAnchor(null);
      setParams((current) => withRange(current, choice), { replace: true });
    },
    [setParams],
  );
  const selectPoint = (key: string) => {
    // The reader asked for this chart: its loading is shown even while an automatic refresh is.
    setAutomaticAnchor(null);
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        next.set("point", key);
        return next;
      },
      { replace: true },
    );
    // The chart can be below the list (a phone): bring it into view without taking the focus.
    document.getElementById("device-history")?.scrollIntoView?.({ block: "nearest" });
  };
  // The one refresh, for the header's button and for the timer: the range's end moves to now and
  // the device, its model, the latest values and the series load again.
  const reload = (automatic: boolean) => {
    // Never the same instant twice: a preset range's series is asked under its end.
    const moved = Math.max(Date.now(), anchor + 1);
    setAnchor(moved);
    setAutomaticAnchor(automatic ? moved : null);
    // The latest values and the series are under the device's key (queries.ts), so this reloads
    // them too; the "Check again" test in DevicePage.test.tsx covers it. A preset range's series
    // is the exception: its end just moved, so the page asks for a new one, and reloading the
    // old one as well would be a request nobody looks at.
    const movesWithAnchor = range.kind === "preset";
    void queryClient.invalidateQueries({
      queryKey: deviceKey(tenant, deviceId),
      predicate: (query) => !(movesWithAnchor && isSeriesKey(query.queryKey)),
    });
    if (modelId !== undefined) void queryClient.invalidateQueries({ queryKey: modelKey(tenant, modelId) });
  };
  const refresh = () => reload(false);
  // Only a preset range that ends "now" follows the clock: a range the user picked by its instants
  // is a fixed span, and a zoomed chart must not jump back to the full range under the pointer.
  // A device that never loaded (not found, or the first load failed) has its own retry.
  useAutoRefresh({
    enabled: device.data !== undefined && range.kind === "preset" && !zoomed,
    since: anchor,
    onRefresh: () => reload(true),
  });

  if (device.isPending) {
    return <DeviceSkeleton label={t("device.loading")} />;
  }
  if (device.isError && classifyError(device.error) === "not_found") {
    return <DeviceNotFound />;
  }
  if (device.isLoadingError) {
    return (
      <div className="grid gap-4">
        <Crumbs />
        <DeviceLoadFailed device={device} />
      </div>
    );
  }
  // Past here the device was loaded; a refresh that failed (`isRefetchError`) keeps it on screen
  // with the failure above the page, and the next refresh tries again.

  const now = Math.max(tick, latest.dataUpdatedAt);
  const health = dataHealth(latest.data, device.data.poll_interval_ms, now);
  const refreshing = (latest.isFetching || device.isFetching) && !quiet;
  const noPoints = model.data !== undefined && latest.data !== undefined && points.length === 0;
  const neverStored =
    latest.data !== undefined && latest.data.points.length === 0 && model.data !== undefined && !noPoints;
  // The model failed and nothing is stored: the points are unknown, so there is nothing to chart.
  // The readings card carries the failure and its retry.
  const unknownPoints = model.isError && points.length === 0;
  // A range in the address bar is a request for the history: older readings may exist beyond the
  // raw period the latest values cover, so a device without recent readings can still show them.
  const askedForHistory = params.has("range") || params.has("from") || params.has("to");
  const info = <DeviceInfo device={device.data} model={model.data} />;

  return (
    <div className="grid gap-5">
      <DeviceHeader
        device={device.data}
        model={model.data}
        health={health}
        now={now}
        refreshing={refreshing}
        updatedAt={latest.dataUpdatedAt}
        quiet={quiet}
        onRefresh={refresh}
      />
      {health.kind === "stale" ? <StaleBanner device={device.data} newest={health.newest} now={now} /> : null}
      {device.isRefetchError ? <DeviceLoadFailed device={device} /> : null}
      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_340px] lg:grid-rows-[auto_1fr] lg:items-start">
        {noPoints || neverStored ? (
          <>
            <div className="grid min-w-0 gap-5 lg:col-start-1 lg:row-span-2 lg:row-start-1">
              {noPoints ? (
                <NoDataPoints model={model.data} />
              ) : latest.data ? (
                <NoReadings
                  device={device.data}
                  model={model.data}
                  latest={latest.data}
                  checking={refreshing}
                  onCheck={refresh}
                  onShowHistory={askedForHistory ? undefined : () => setRange({ kind: "preset", preset: "90d" })}
                />
              ) : null}
              {neverStored && askedForHistory ? (
                <HistoryCard
                  deviceId={deviceId}
                  deviceName={device.data.name}
                  tenant={tenant}
                  points={keys}
                  point={point}
                  range={range}
                  anchor={anchor}
                  onRangeChange={setRange}
                  pointsPending={model.isPending}
                  quiet={quiet}
                  onZoomChange={setZoomed}
                  onFetchingChange={setHistoryFetching}
                />
              ) : null}
            </div>
            <div className="lg:col-start-2 lg:row-start-1">{info}</div>
          </>
        ) : (
          <>
            <div className={unknownPoints ? "lg:col-start-1 lg:row-start-1" : "lg:col-start-2 lg:row-start-1"}>
              <LatestCard
                model={model}
                latest={latest}
                selected={point}
                onSelect={selectPoint}
                pollIntervalMs={device.data.poll_interval_ms}
                now={now}
              />
            </div>
            {unknownPoints ? null : (
              <div className="min-w-0 lg:col-start-1 lg:row-span-2 lg:row-start-1">
                <HistoryCard
                  deviceId={deviceId}
                  deviceName={device.data.name}
                  tenant={tenant}
                  points={keys}
                  point={point}
                  range={range}
                  anchor={anchor}
                  onRangeChange={setRange}
                  pointsPending={model.isPending}
                  quiet={quiet}
                  onZoomChange={setZoomed}
                  onFetchingChange={setHistoryFetching}
                />
              </div>
            )}
            <div className={unknownPoints ? "lg:col-start-2 lg:row-start-1" : "lg:col-start-2 lg:row-start-2"}>
              {info}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/** The device could not be loaded, or not be loaded again: what failed, and a button that asks again. */
function DeviceLoadFailed({ device }: { device: UseQueryResult<Device> }) {
  const { t, errorMessage } = useI18n();
  return (
    <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
      <AlertDescription className="col-start-1">
        <p>{t("device.load_failed", { reason: errorMessage(device.error) })}</p>
      </AlertDescription>
      <Button variant="outline" onClick={() => void device.refetch()} disabled={device.isFetching}>
        {t("page.action.retry")}
      </Button>
    </Alert>
  );
}

/** The trail on a page that has no title yet: back to the devices. */
function Crumbs() {
  const { t } = useI18n();
  return (
    <nav aria-label={t("page.breadcrumbs")}>
      <ol className="flex items-center gap-1 text-sm text-muted-foreground">
        <li className="flex items-center gap-1">
          <ChevronLeftIcon aria-hidden="true" className="size-3.5" />
          <Link
            to={DEVICES_PATH}
            className={cn("rounded-sm underline-offset-4 hover:text-foreground hover:underline", focusRing)}
          >
            {t("devices.title")}
          </Link>
        </li>
      </ol>
    </nav>
  );
}

/** The page's shape while the device loads: the head, and the cards where they will be. */
function DeviceSkeleton({ label }: { label: string }) {
  const block = "animate-pulse rounded-md bg-muted";
  return (
    <div role="status" aria-busy="true" className="grid gap-5">
      <span className="sr-only">{label}</span>
      <div aria-hidden="true" className="grid gap-2.5">
        <div className={cn(block, "h-4 w-24")} />
        <div className={cn(block, "h-8 w-56")} />
        <div className={cn(block, "h-4 w-72 max-w-full")} />
      </div>
      <div aria-hidden="true" className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_340px] lg:items-start">
        <div className={cn(block, "h-64 lg:col-start-2 lg:row-start-1")} />
        <div className={cn(block, "h-[28rem] lg:col-start-1 lg:row-start-1")} />
      </div>
    </div>
  );
}

/** No such device in the active tenant: the next step is the right organization, or another device. */
function DeviceNotFound() {
  const { t } = useI18n();
  const active = useSession().session?.activeTenant;
  const back = useBackToConversation(true);
  return (
    <StatusPage
      title={t("device.not_found.title")}
      icon={<CpuIcon />}
      actions={
        <>
          <Button asChild>
            <Link to={DEVICES_PATH}>
              {t("device.not_found.all_devices")}
              <ChevronRightIcon aria-hidden="true" />
            </Link>
          </Button>
          <Button asChild variant="outline">
            <Link to={back}>{t("shell.back")}</Link>
          </Button>
        </>
      }
    >
      <p>{t("device.not_found.body", { tenant: active ? tenantName(active, t) : "" })}</p>
    </StatusPage>
  );
}
