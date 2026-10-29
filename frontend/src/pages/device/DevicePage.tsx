import { useQuery, useQueryClient } from "@tanstack/react-query";
import { CpuIcon, RefreshCwIcon } from "lucide-react";
import { useCallback, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { Device, DeviceModel } from "../../api/devices.ts";
import { classifyError } from "../../api/errors.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantName } from "../../session/tenant-name.ts";
import { StatusPage } from "../StatusPage.tsx";
import { useDeviceFormat } from "./format.ts";
import { HistoryCard } from "./HistoryCard.tsx";
import { LatestCard } from "./LatestCard.tsx";
import { chartablePoints, choosePoint } from "./points.ts";
import { deviceKey, latestKey, modelKey } from "./queries.ts";
import { parseRange, type RangeChoice, withRange } from "./time.ts";

/** The device IDs the API hands out: lowercase hyphenated UUIDs (`DeviceId` in api/openapi.json). */
const DEVICE_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * One device (`/devices/:deviceId`, checklist I3): what it is, the newest stored value of each of
 * its data points, and a chart of one data point's stored history. The chart's data point and
 * range live in the address bar (`point`, `range` or `from` and `to`), so a link opens the same
 * view. Live values, health and CSV export come with Laysi/trustiot-v2#454.
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
  // Until the model answers, the chart waits: its points decide which one to show first.
  const keys = model.isPending ? [] : chartablePoints(model.data, latest.data);
  const point = choosePoint(params.get("point"), keys);

  const setRange = useCallback(
    (choice: RangeChoice) => {
      setAnchor(Date.now());
      setParams((current) => withRange(current, choice), { replace: true });
    },
    [setParams],
  );
  const setPoint = useCallback(
    (key: string) => {
      setParams(
        (current) => {
          const next = new URLSearchParams(current);
          next.set("point", key);
          return next;
        },
        { replace: true },
      );
    },
    [setParams],
  );
  const refresh = () => {
    setAnchor(Date.now());
    void queryClient.invalidateQueries({ queryKey: deviceKey(tenant, deviceId) });
    if (modelId !== undefined) void queryClient.invalidateQueries({ queryKey: modelKey(tenant, modelId) });
  };

  if (device.isPending) {
    return <Spinner label={t("device.loading")} />;
  }
  if (device.isError) {
    if (classifyError(device.error) === "not_found") {
      return <DeviceNotFound />;
    }
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

  const refreshing = latest.isFetching || device.isFetching;
  return (
    <div className="grid gap-6">
      <DeviceHeader
        device={device.data}
        model={model.data}
        refreshing={refreshing}
        updatedAt={latest.dataUpdatedAt}
        onRefresh={refresh}
      />
      <LatestCard
        device={device.data}
        model={model}
        latest={latest}
        selected={point}
        onShowChart={(key) => {
          setPoint(key);
          document.getElementById("device-history")?.focus();
        }}
      />
      <HistoryCard
        deviceId={deviceId}
        tenant={tenant}
        points={keys}
        point={point}
        onPointChange={setPoint}
        range={range}
        anchor={anchor}
        onRangeChange={setRange}
        pointsPending={model.isPending}
      />
    </div>
  );
}

function DeviceHeader({
  device,
  model,
  refreshing,
  updatedAt,
  onRefresh,
}: {
  device: Device;
  model: DeviceModel | undefined;
  refreshing: boolean;
  updatedAt: number;
  onRefresh(): void;
}) {
  const format = useDeviceFormat();
  const { t } = format;
  const transport = device.transport === "modbus_tcp" ? "Modbus TCP" : "Modbus RTU over TCP";
  const facts: { label: string; value: string; mono?: boolean }[] = [
    { label: t("device.info.model"), value: model?.name ?? "…", mono: true },
    { label: t("device.info.gateway"), value: device.hardware_id, mono: true },
    {
      label: t("device.info.connection"),
      value: t("device.info.connection_value", {
        transport,
        address: `${device.host}:${device.port}`,
        unit: device.unit_id,
      }),
    },
    {
      label: t("device.info.poll"),
      value: t("device.info.poll_every", { duration: format.duration(device.poll_interval_ms) }),
    },
  ];
  return (
    <header className="grid gap-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="grid min-w-0 gap-1">
          <p className="text-sm font-medium text-muted-foreground">{t("device.eyebrow")}</p>
          <h1 className="font-mono text-2xl font-semibold tracking-tight break-all">{device.name}</h1>
        </div>
        <div className="flex items-center gap-3">
          <p role="status" className="text-sm text-muted-foreground">
            {refreshing
              ? t("device.refreshing")
              : updatedAt > 0
                ? t("device.updated_at", { time: format.time(updatedAt) })
                : null}
          </p>
          <Button variant="outline" onClick={onRefresh} disabled={refreshing}>
            <RefreshCwIcon className={refreshing ? "animate-spin" : undefined} />
            {t("device.refresh")}
          </Button>
        </div>
      </div>
      <dl className="grid gap-x-6 gap-y-3 rounded-xl border bg-card px-5 py-4 text-sm sm:grid-cols-2 lg:grid-cols-4">
        {facts.map((fact) => (
          <div key={fact.label} className="grid min-w-0 gap-0.5">
            <dt className="text-muted-foreground">{fact.label}</dt>
            <dd className={fact.mono ? "font-mono break-all" : "break-words"}>{fact.value}</dd>
          </div>
        ))}
      </dl>
    </header>
  );
}

/** No such device in the active tenant: the next step is the right organization, or the home page. */
function DeviceNotFound() {
  const { t } = useI18n();
  const active = useSession().session?.activeTenant;
  return (
    <StatusPage
      title={t("device.not_found.title")}
      icon={<CpuIcon />}
      actions={
        <Button asChild variant="outline">
          <Link to="/">{t("page.action.home")}</Link>
        </Button>
      }
    >
      <p>{t("device.not_found.body", { tenant: active ? tenantName(active, t) : "" })}</p>
    </StatusPage>
  );
}
