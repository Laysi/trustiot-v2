import { type UseQueryResult, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo } from "react";

import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import type { DeviceModel } from "../../api/devices.ts";
import type { I18n } from "../../i18n/useI18n.ts";
import {
  deviceListKey,
  modelKey,
  modelListKey,
  tenantDevicesKey,
  tenantModelsKey,
} from "../../pages/device/queries.ts";
import type { MentionKind, MentionTarget } from "./text.ts";
import { describeRange, type TimeRange, timeRanges } from "./time-ranges.ts";

// What a mention can stand for, read from the tenant's own lists: the devices, the models and,
// through each model in use, its data points. The lists are the ones the device pages use (same
// endpoints and query keys, so one cache), asked only while a list is open, and filtered here: the
// API has no search. Every key starts with the active tenant's ID, so another tenant's answer is
// never in these lists (a tenant switch also remounts the page and drops the tenant's queries).

/** One thing the list offers. `key` is unique and stable, so the selection and the recents can name it. */
export interface Entry {
  key: string;
  kind: MentionKind;
  /** What follows the `@` in the box. */
  name: string;
  /** The line under the name. */
  meta: string;
  /** More words the search looks at (a data point's key). */
  words?: string;
  target: MentionTarget;
}

export type SourceStatus = "loading" | "ready" | "error";

export interface Sources {
  devices: Entry[];
  points: Entry[];
  models: Entry[];
  status: { devices: SourceStatus; points: SourceStatus; models: SourceStatus };
  /** Asks again for what failed. */
  retry(): void;
}

/** What a list needs: which of the lookups to run. */
export interface Wants {
  devices: boolean;
  points: boolean;
  models: boolean;
}

/** What the model queries amount to: the models that arrived, and whether any failed or all are in. */
function combineDetails(results: UseQueryResult<DeviceModel>[]) {
  return {
    models: results.flatMap((result) => result.data ?? []),
    failed: results.some((result) => result.isError),
    settled: results.every((result) => result.isSuccess),
  };
}

/** How long an answer is reused before the next `@` asks again. */
const STALE_MS = 30_000;

type Translate = I18n["t"];

/** `60000` as "60 s", `900000` as "15 min". */
export function formatInterval(ms: number, t: Translate): string {
  if (ms % 3_600_000 === 0) return t("chat.mention.interval_hours", { value: ms / 3_600_000 });
  if (ms % 60_000 === 0) return t("chat.mention.interval_minutes", { value: ms / 60_000 });
  return t("chat.mention.interval_seconds", { value: Math.round(ms / 100) / 10 });
}

export function useMentionSources(tenant: string, wants: Wants, t: Translate): Sources {
  const api = useDeviceApi();
  const needDevices = wants.devices || wants.points || wants.models;
  const devicesQuery = useQuery({
    queryKey: deviceListKey(tenant),
    queryFn: ({ signal }) => api.listDevices(signal),
    enabled: needDevices,
    staleTime: STALE_MS,
  });
  const modelsQuery = useQuery({
    queryKey: modelListKey(tenant),
    queryFn: ({ signal }) => api.listDeviceModels(signal),
    enabled: needDevices,
    staleTime: STALE_MS,
  });
  const devices = devicesQuery.data;
  const modelIds = useMemo(() => [...new Set((devices ?? []).map((device) => device.model_id))].sort(), [devices]);
  const queryClient = useQueryClient();
  const detailQueries = useQueries({
    queries: modelIds.map((modelId) => ({
      queryKey: modelKey(tenant, modelId),
      queryFn: ({ signal }: { signal: AbortSignal }) => api.getDeviceModel(modelId, signal),
      enabled: wants.points,
      staleTime: STALE_MS,
    })),
    combine: combineDetails,
  });

  const entries = useMemo(() => {
    const modelNames = new Map((modelsQuery.data ?? []).map((model) => [model.model_id, model.name]));
    const deviceEntries: Entry[] = (devices ?? []).map((device) => {
      const interval = formatInterval(device.poll_interval_ms, t);
      const model = modelNames.get(device.model_id);
      return {
        key: `device:${device.device_id}`,
        kind: "device",
        name: device.name,
        meta: model
          ? t("chat.mention.device_meta", { model, interval })
          : t("chat.mention.device_meta_plain", { interval }),
        target: { kind: "device", deviceId: device.device_id },
      };
    });
    const byModel = new Map<string, DeviceModel>();
    for (const model of detailQueries.models) byModel.set(model.model_id, model);
    const pointEntries: Entry[] = (devices ?? []).flatMap((device) =>
      (byModel.get(device.model_id)?.points ?? []).map((point): Entry => {
        const interval = formatInterval(device.poll_interval_ms, t);
        const label = point.description?.trim() || point.key;
        return {
          key: `point:${device.device_id}:${point.key}`,
          kind: "point",
          name: `${device.name} · ${label}`,
          meta: t("chat.mention.point_meta", { key: point.key, interval }),
          words: point.key,
          target: { kind: "point", deviceId: device.device_id, point: point.key },
        };
      }),
    );
    const deviceCount = new Map<string, number>();
    for (const device of devices ?? []) deviceCount.set(device.model_id, (deviceCount.get(device.model_id) ?? 0) + 1);
    const modelEntries: Entry[] = (modelsQuery.data ?? []).map((model) => ({
      key: `model:${model.model_id}`,
      kind: "model",
      name: model.name,
      meta:
        devices === undefined
          ? t("chat.mention.model_meta_points", { points: model.point_count })
          : t("chat.mention.model_meta", {
              devices: deviceCount.get(model.model_id) ?? 0,
              points: model.point_count,
            }),
      target: { kind: "model", modelId: model.model_id },
    }));
    return { deviceEntries, pointEntries, modelEntries };
  }, [devices, modelsQuery.data, detailQueries.models, t]);

  const settle = (queries: { isError: boolean; isSuccess: boolean }[]): SourceStatus =>
    queries.some((query) => query.isError) ? "error" : queries.every((query) => query.isSuccess) ? "ready" : "loading";
  const devicesStatus = settle([devicesQuery]);
  const pointsStatus: SourceStatus =
    devicesStatus === "error" || detailQueries.failed
      ? "error"
      : devicesStatus === "ready" && detailQueries.settled
        ? "ready"
        : "loading";
  const status = {
    devices: wants.devices ? devicesStatus : "ready",
    models: wants.models ? settle([modelsQuery, devicesQuery]) : "ready",
    points: wants.points ? pointsStatus : "ready",
  } satisfies Sources["status"];

  // Asks again for what failed, and only that: the devices page keeps the same keys.
  const retry = () => {
    for (const root of [tenantDevicesKey(tenant), tenantModelsKey(tenant)]) {
      void queryClient.refetchQueries({ queryKey: root, predicate: (query) => query.state.status === "error" });
    }
  };

  return {
    devices: entries.deviceEntries,
    points: entries.pointEntries,
    models: entries.modelEntries,
    status,
    retry,
  };
}

/** The time ranges as entries, worked out at `now` in the user's time zone. */
export function rangeEntries(now: number, timeZone: string, locale: string, t: Translate): Entry[] {
  return timeRanges(now, timeZone).map((range) => rangeEntry(range, now, timeZone, locale, t));
}

function rangeEntry(range: TimeRange, now: number, timeZone: string, locale: string, t: Translate): Entry {
  return {
    key: `range:${range.id}`,
    kind: "range",
    name: t(`chat.mention.range.${range.id}`),
    meta: describeRange(range, locale, timeZone, t("chat.mention.range.now")),
    target: {
      kind: "range",
      id: range.id,
      from: new Date(range.from).toISOString(),
      to: new Date(range.open ? now : range.to).toISOString(),
    },
  };
}
