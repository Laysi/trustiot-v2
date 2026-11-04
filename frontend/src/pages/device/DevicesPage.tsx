import { useQuery } from "@tanstack/react-query";
import { CpuIcon, SearchIcon, SearchXIcon, XIcon } from "lucide-react";
import { type ReactNode, useEffect, useId, useMemo, useState } from "react";
import { Link, useSearchParams } from "react-router";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { Device } from "../../api/devices.ts";
import { devicePath } from "../../app/paths.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { useDeviceFormat } from "./format.ts";
import { deviceListKey, modelListKey } from "./queries.ts";

/** The filters live in the address bar, so a link or a reload keeps them. */
const SEARCH_PARAM = "q";
const GATEWAY_PARAM = "gateway";
const MODEL_PARAM = "model";

/** The names the table and the phone's list print for a device's transport. */
function transportLabel(transport: Device["transport"]): string {
  return transport === "modbus_tcp" ? "Modbus TCP" : "Modbus RTU over TCP";
}

/**
 * The tenant's devices (`/devices`, checklist C2), read-only: every role sees the viewer's version
 * of docs/design/tenant-admin/ (`devices--devices-viewer`), without the add button and the row
 * actions. The API returns every device at once, so search and the gateway and model filters run
 * here (design decision 6). The status column waits for the health data (Laysi/trustiot-v2#118
 * and #454); the "gateway" column shows the device's `hardware_id`, the same identifier the
 * device page shows, because no API gives the SPA a gateway's name yet.
 */
export function DevicesPage() {
  const { t, errorMessage } = useI18n();
  const tenant = useSession().session?.activeTenant;
  const tenantId = tenant?.id ?? "";
  const api = useDeviceApi();
  const [params, setParams] = useSearchParams();
  const searchId = useId();

  const gateway = params.get(GATEWAY_PARAM) ?? "";
  const modelFilter = params.get(MODEL_PARAM) ?? "";
  // The box keeps what is typed (an input method may hold text back); the address bar gets each change.
  const [query, setQuery] = useState(() => params.get(SEARCH_PARAM) ?? "");
  // Going back or forward (or following a link) to another search puts it in the box; typing already matches the address.
  const urlQuery = params.get(SEARCH_PARAM) ?? "";
  useEffect(() => {
    setQuery((current) => (current.trim() === urlQuery.trim() ? current : urlQuery));
  }, [urlQuery]);

  const devices = useQuery({
    queryKey: deviceListKey(tenantId),
    queryFn: ({ signal }) => api.listDevices(signal),
  });
  const models = useQuery({
    queryKey: modelListKey(tenantId),
    queryFn: ({ signal }) => api.listDeviceModels(signal),
  });

  const modelNames = useMemo(
    () => new Map((models.data ?? []).map((model) => [model.model_id, model.name])),
    [models.data],
  );
  const gateways = useMemo(
    () => [...new Set((devices.data ?? []).map((device) => device.hardware_id))].sort(),
    [devices.data],
  );

  const setParam = (name: string, value: string) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value === "") next.delete(name);
        else next.set(name, value);
        return next;
      },
      { replace: true },
    );
  const changeQuery = (value: string) => {
    setQuery(value);
    setParam(SEARCH_PARAM, value.trim());
  };
  const clearAll = () => {
    setQuery("");
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        for (const name of [SEARCH_PARAM, GATEWAY_PARAM, MODEL_PARAM]) next.delete(name);
        return next;
      },
      { replace: true },
    );
  };

  // What the address bar says, not the box: a reload and a typed link filter the same way.
  const search = (params.get(SEARCH_PARAM) ?? "").trim().toLowerCase();
  const all = devices.data ?? [];
  const shown = all.filter(
    (device) =>
      (search === "" || device.name.toLowerCase().includes(search) || device.host.toLowerCase().includes(search)) &&
      (gateway === "" || device.hardware_id === gateway) &&
      (modelFilter === "" || device.model_id === modelFilter),
  );
  const modelLabel = (modelId: string) => modelNames.get(modelId) ?? (models.isPending ? "…" : "—");

  const chips: { id: string; text: string; remove(): void }[] = [];
  if (search !== "") {
    chips.push({
      id: SEARCH_PARAM,
      text: t("devices.filter.search_chip", { value: (params.get(SEARCH_PARAM) ?? "").trim() }),
      remove: () => changeQuery(""),
    });
  }
  if (gateway !== "") {
    chips.push({
      id: GATEWAY_PARAM,
      text: t("devices.filter.gateway_chip", { value: gateway }),
      remove: () => setParam(GATEWAY_PARAM, ""),
    });
  }
  if (modelFilter !== "") {
    chips.push({
      id: MODEL_PARAM,
      text: t("devices.filter.model_chip", { value: modelLabel(modelFilter) }),
      remove: () => setParam(MODEL_PARAM, ""),
    });
  }

  let body: ReactNode;
  if (devices.isPending) {
    body = <ListSkeleton label={t("devices.loading")} />;
  } else if (devices.isError) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("devices.load_failed", { reason: errorMessage(devices.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void devices.refetch()} disabled={devices.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (all.length === 0) {
    // Without the model list nothing is known about the second step, so it is left unmarked.
    body = <SetupSteps hasModels={models.isSuccess ? models.data.length > 0 : undefined} />;
  } else if (shown.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <p className="font-semibold">{t("devices.no_match.title")}</p>
        <p className="text-sm text-muted-foreground">{t("devices.no_match.body")}</p>
      </div>
    );
  } else {
    body = <DeviceList devices={shown} modelLabel={modelLabel} />;
  }

  // The toolbar has nothing to filter until there are devices.
  const showToolbar = devices.isSuccess && all.length > 0;
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={t("devices.title")} meta={t("devices.description")} />

      {showToolbar ? (
        <div className="grid gap-3">
          <search className="flex flex-wrap items-center gap-3">
            <div className="relative w-full sm:w-72">
              <label htmlFor={searchId} className="sr-only">
                {t("devices.search.label")}
              </label>
              <SearchIcon
                aria-hidden="true"
                className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
              />
              <input
                id={searchId}
                type="search"
                value={query}
                placeholder={t("devices.search.placeholder")}
                className={cn(controlClasses, "pl-9")}
                onChange={(event) => changeQuery(event.target.value)}
              />
            </div>
            <div className="w-full sm:w-52">
              <label htmlFor={`${searchId}-gateway`} className="sr-only">
                {t("devices.filter.gateway")}
              </label>
              <NativeSelect
                id={`${searchId}-gateway`}
                value={gateway}
                onChange={(event) => setParam(GATEWAY_PARAM, event.target.value)}
              >
                <option value="">{t("devices.filter.all_gateways")}</option>
                {/* A gateway named in the address that has no device still shows, so the select says what is filtered. */}
                {(gateway !== "" && !gateways.includes(gateway) ? [...gateways, gateway] : gateways).map((id) => (
                  <option key={id} value={id}>
                    {id}
                  </option>
                ))}
              </NativeSelect>
            </div>
            <div className="w-full sm:w-52">
              <label htmlFor={`${searchId}-model`} className="sr-only">
                {t("devices.filter.model")}
              </label>
              <NativeSelect
                id={`${searchId}-model`}
                value={modelFilter}
                onChange={(event) => setParam(MODEL_PARAM, event.target.value)}
              >
                <option value="">{t("devices.filter.all_models")}</option>
                {(models.data ?? []).map((model) => (
                  <option key={model.model_id} value={model.model_id}>
                    {model.name}
                  </option>
                ))}
                {modelFilter !== "" && !modelNames.has(modelFilter) ? (
                  <option value={modelFilter}>{modelLabel(modelFilter)}</option>
                ) : null}
              </NativeSelect>
            </div>
          </search>
          {chips.length > 0 ? (
            <div className="flex flex-wrap items-center gap-2 text-sm">
              <span className="text-muted-foreground">{t("devices.filter.active")}</span>
              {chips.map((chip) => (
                <button
                  key={chip.id}
                  type="button"
                  aria-label={t("devices.filter.remove", { filter: chip.text })}
                  className={cn(
                    "inline-flex max-w-full items-center gap-1 rounded-sm bg-secondary px-2 py-0.5 text-xs font-medium text-secondary-foreground hover:bg-secondary/80",
                    focusRing,
                  )}
                  onClick={chip.remove}
                >
                  <span className="truncate">{chip.text}</span>
                  <XIcon aria-hidden="true" className="size-3.5 shrink-0" />
                </button>
              ))}
              <Button variant="ghost" size="sm" onClick={clearAll}>
                {t("devices.filter.clear")}
              </Button>
            </div>
          ) : null}
        </div>
      ) : null}

      {body}

      {models.isError && devices.isSuccess ? (
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{t("devices.models_failed", { reason: errorMessage(models.error) })}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void models.refetch()} disabled={models.isFetching}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      ) : null}

      {showToolbar ? (
        <p role="status" className="text-xs text-muted-foreground">
          {t("devices.count", { shown: shown.length, total: all.length })}
        </p>
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
      {[0, 1, 2, 3, 4].map((row) => (
        <div key={row} aria-hidden="true" className="flex items-center justify-between gap-4">
          <span className="h-3 w-[30%] animate-pulse rounded bg-muted" />
          <span className="h-3 w-[14%] animate-pulse rounded bg-muted" />
          <span className="h-3 w-[12%] animate-pulse rounded bg-muted" />
          <span className="h-3 w-[22%] animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}

/**
 * The tenant has no device: the three steps of devices--devices-empty-setup as text. The pages
 * that do each step do not exist yet (Laysi/trustiot-v2#126, #127), so the steps have no
 * buttons. Only what the data shows is marked: a model exists or not; whether a gateway is bound
 * is not known to this page, so the first step carries no mark.
 */
function SetupSteps({ hasModels }: { hasModels: boolean | undefined }) {
  const { t } = useI18n();
  const steps: { title: string; body?: string; mark?: string }[] = [
    { title: t("devices.setup.step_gateway") },
    {
      title: t("devices.setup.step_model"),
      body: t("devices.setup.step_model_body"),
      ...(hasModels === undefined ? {} : { mark: t(hasModels ? "devices.setup.done" : "devices.setup.next") }),
    },
    { title: t("devices.setup.step_device"), ...(hasModels ? { mark: t("devices.setup.next") } : {}) },
  ];
  return (
    <section className="grid gap-4 rounded-lg border bg-card px-4 py-8 text-card-foreground sm:px-6">
      <div className="grid justify-items-start gap-2">
        <CpuIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <h2 className="font-semibold">{t("devices.setup.title")}</h2>
        <p className="text-sm text-muted-foreground">{t("devices.setup.body")}</p>
      </div>
      <ol aria-label={t("devices.setup.steps_label")} className="grid gap-3">
        {steps.map((step, index) => (
          <li key={step.title} className="grid grid-cols-[1.5rem_1fr] items-start gap-x-3">
            <span
              aria-hidden="true"
              className="grid size-6 place-items-center rounded-full bg-muted text-xs font-medium text-muted-foreground"
            >
              {index + 1}
            </span>
            <div className="grid gap-1">
              <p className="text-sm font-semibold">
                {step.title}
                {step.mark ? <span className="ml-1 font-normal text-muted-foreground">{step.mark}</span> : null}
              </p>
              {step.body ? <p className="text-sm text-muted-foreground">{step.body}</p> : null}
            </div>
          </li>
        ))}
      </ol>
      <p className="text-sm text-muted-foreground">{t("devices.setup.note")}</p>
    </section>
  );
}

/**
 * The devices as a table on a wide screen and as a list on a phone, the two forms the design
 * draws; the one that does not fit is hidden, so a screen reader reads one of them.
 */
function DeviceList({ devices, modelLabel }: { devices: readonly Device[]; modelLabel(modelId: string): string }) {
  const format = useDeviceFormat();
  const { t } = format;
  const connection = (device: Device) =>
    t("devices.connection", {
      transport: transportLabel(device.transport),
      address: `${device.host}:${device.port}`,
      unit: device.unit_id,
    });
  const link = (device: Device) => (
    <Link
      to={devicePath(device.device_id)}
      className={cn("rounded-sm font-medium wrap-anywhere underline-offset-4 hover:underline", focusRing)}
    >
      {device.name}
    </Link>
  );
  return (
    <section aria-label={t("devices.list_label")} className="rounded-lg border bg-card text-card-foreground">
      <table className="hidden w-full text-sm sm:table">
        <caption className="sr-only">{t("devices.caption", { count: devices.length })}</caption>
        <thead className="bg-muted/60 text-left text-muted-foreground">
          <tr>
            <th scope="col" className="px-4 py-2.5 font-medium">
              {t("devices.column.device")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("devices.column.gateway")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("devices.column.connection")}
            </th>
            <th scope="col" className="px-4 py-2.5 font-medium whitespace-nowrap">
              {t("devices.column.poll")}
            </th>
          </tr>
        </thead>
        <tbody>
          {devices.map((device) => (
            <tr key={device.device_id} className="border-t">
              <th scope="row" className="max-w-64 px-4 py-3 text-left font-normal">
                <span className="grid justify-items-start gap-1">
                  {link(device)}
                  <span className="text-xs text-muted-foreground">
                    {t("devices.model_of", { model: modelLabel(device.model_id) })}
                  </span>
                </span>
              </th>
              <td className="px-3 py-3 font-mono whitespace-nowrap">{device.hardware_id}</td>
              <td className="max-w-72 px-3 py-3 text-xs wrap-anywhere text-muted-foreground">{connection(device)}</td>
              <td className="px-4 py-3 whitespace-nowrap">
                {t("device.info.poll_every", { duration: format.duration(device.poll_interval_ms) })}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <ul aria-label={t("devices.caption", { count: devices.length })} className="grid divide-y sm:hidden">
        {devices.map((device) => (
          <li key={device.device_id} className="grid gap-1.5 px-4 py-3 text-sm">
            {link(device)}
            <div className="flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-muted-foreground">
              <span>{t("devices.model_of", { model: modelLabel(device.model_id) })}</span>
              <span>{t("devices.gateway_of", { gateway: device.hardware_id })}</span>
              <span>{t("device.info.poll_every", { duration: format.duration(device.poll_interval_ms) })}</span>
            </div>
            <p className="text-xs wrap-anywhere text-muted-foreground">{connection(device)}</p>
          </li>
        ))}
      </ul>
    </section>
  );
}
