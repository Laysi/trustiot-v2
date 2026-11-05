import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronLeftIcon, ChevronRightIcon, CpuIcon } from "lucide-react";
import { useCallback, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
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
import { DeviceHeader, StaleBanner } from "./DeviceHeader.tsx";
import { DeviceInfo, NoDataPoints, NoReadings } from "./DeviceInfo.tsx";
import { HistoryCard } from "./HistoryCard.tsx";
import { dataHealth } from "./health.ts";
import { LatestCard } from "./LatestCard.tsx";
import { chartablePoints, choosePoint } from "./points.ts";
import { deviceKey, latestKey, modelKey } from "./queries.ts";
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
  const { t, errorMessage } = useI18n();
  const api = useDeviceApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const [params, setParams] = useSearchParams();
  // A preset range ends at this instant: set when the page opens, the range changes, or the
  // user refreshes, never on a render (a moving end would refetch forever).
  const [anchor, setAnchor] = useState(() => Date.now());
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

  const range = parseRange(params);
  const points = chartablePoints(model.data, latest.data);
  // Until the model answers, the chart waits: its points decide which one to show first.
  const keys = model.isPending ? [] : points;
  const point = choosePoint(params.get("point"), keys);

  const setRange = useCallback(
    (choice: RangeChoice) => {
      setAnchor(Date.now());
      setParams((current) => withRange(current, choice), { replace: true });
    },
    [setParams],
  );
  const selectPoint = (key: string) => {
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
  const refresh = () => {
    setAnchor(Date.now());
    // The latest values and the series are under the device's key (queries.ts), so this reloads
    // them too; the "Check again" test in DevicePage.test.tsx covers it.
    void queryClient.invalidateQueries({ queryKey: deviceKey(tenant, deviceId) });
    if (modelId !== undefined) void queryClient.invalidateQueries({ queryKey: modelKey(tenant, modelId) });
  };

  if (device.isPending) {
    return <DeviceSkeleton label={t("device.loading")} />;
  }
  if (device.isError) {
    if (classifyError(device.error) === "not_found") {
      return <DeviceNotFound />;
    }
    return (
      <div className="grid gap-4">
        <Crumbs />
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{t("device.load_failed", { reason: errorMessage(device.error) })}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void device.refetch()} disabled={device.isFetching}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      </div>
    );
  }

  const now = Math.max(tick, latest.dataUpdatedAt);
  const health = dataHealth(latest.data, device.data.poll_interval_ms, now);
  const refreshing = latest.isFetching || device.isFetching;
  const noPoints = model.isSuccess && latest.isSuccess && points.length === 0;
  const neverStored = latest.isSuccess && latest.data.points.length === 0 && model.isSuccess && !noPoints;
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
        onRefresh={refresh}
      />
      {health.kind === "stale" ? <StaleBanner device={device.data} newest={health.newest} now={now} /> : null}
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
                  tenant={tenant}
                  points={keys}
                  point={point}
                  range={range}
                  anchor={anchor}
                  onRangeChange={setRange}
                  pointsPending={model.isPending}
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
                  tenant={tenant}
                  points={keys}
                  point={point}
                  range={range}
                  anchor={anchor}
                  onRangeChange={setRange}
                  pointsPending={model.isPending}
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
