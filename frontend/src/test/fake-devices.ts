import { type AssetsApi, assetsApi } from "../api/assets.ts";
import { createApiClient } from "../api/client.ts";
import {
  type Device,
  type DeviceApi,
  type DeviceModel,
  deviceApi,
  type Interval,
  type Reading,
  type Series,
  type SeriesPoint,
} from "../api/devices.ts";
import { INTERVAL_MS, offsetAt } from "../pages/device/time.ts";
import type { HttpPreviewDeclaration } from "../ui-preview/contract.ts";
import { FakeAssets } from "./assets-rules.ts";

/**
 * The assets and telemetry-query endpoints the device page reads, held in memory and answered at
 * the HTTP level: the page goes through the real API client (`deviceApi`, zod parsing, the query
 * string), and this fake answers with the bodies api/openapi.json describes, by the rules of
 * services/telemetry-query/README.md 「API」:
 *
 * - `latest`: each point's newest reading from the raw period on, ordered by point.
 * - `series`: the finest aggregated interval with at most 1500 buckets (never `raw`), buckets
 *   whose start is in `[from, to)`, local hours and midnights in `tz`; a range wholly before
 *   the aggregated period is 403 `telemetry.outside_queryable_period` with `details.earliest`,
 *   one that starts before it is cut there with `truncated`.
 *
 * Every request is recorded; `failNext` answers the next request to an endpoint with an error.
 */

type Endpoint = "devices" | "models" | "device" | "model" | "latest" | "series";

interface StoredReading {
  device: string;
  point: string;
  time: number;
  value: Reading;
}

const AGGREGATED: readonly Interval[] = ["1m", "5m", "15m", "1h", "3h", "6h", "12h", "1d"];
const MAX_BUCKETS = 1500;
const DAY = 86_400_000;

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

function error(status: number, code: string, messageKey: string, details?: Record<string, string | number>) {
  return json(status, { code, message_key: messageKey, ...(details ? { details } : {}) });
}

function byName(a: { name: string }, b: { name: string }): number {
  return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
}

let ids = 0;
/**
 * A fresh lowercase UUID (the API's form), unique within the test process. The counter is shared by
 * every test file that runs in the same `bun test` process, so a test cannot know which values it
 * has reached. Generated IDs therefore live in their own range, `00000000-0000-4000-a000-…`, and
 * the IDs tests write by hand (an ID that must be unknown, a fixed user) use `00000000-0000-4000-8000-…`,
 * which this never returns: a hand-written "unknown" ID that was in the generated range became
 * a real record whenever earlier files had used just enough IDs (Laysi/trustiot-v2#1176).
 */
export function fakeUuid(): string {
  ids += 1;
  return `00000000-0000-4000-a000-${ids.toString(16).padStart(12, "0")}`;
}

export class FakeDeviceServer {
  readonly devices = new Map<string, Device>();
  readonly models = new Map<string, DeviceModel>();
  readonly readings: StoredReading[] = [];
  /** Every request's URL, in order. */
  readonly requests: URL[] = [];
  /** The writes and form reads of the model and device screens (assets-rules.ts), on these devices and models. */
  readonly assets = new FakeAssets({
    devices: this.devices,
    models: this.models,
    now: () => this.now(),
    newId: () => fakeUuid(),
    addModel: (name) => this.addModel(name),
  });
  /** The server's clock. */
  now: () => number = () => Date.now();
  /** How far back the raw readings (latest values) and the aggregates (series) can be queried. */
  rawPeriodMs = 7 * DAY;
  aggregatedPeriodMs = 92 * DAY;
  private readonly failures = new Map<Endpoint, Response[]>();

  /**
   * A model with these points, and a device of it; returns the device. `modelId` puts the device
   * on a model made before (the points are then ignored).
   */
  addDevice(
    options: {
      name?: string;
      modelName?: string;
      modelId?: string;
      points?: {
        key: string;
        data_type: DeviceModel["points"][number]["data_type"];
        description?: string;
        unit?: string;
      }[];
      hardwareId?: string;
      transport?: Device["transport"];
      host?: string;
      port?: number;
      unitId?: number;
      pollIntervalMs?: number;
    } = {},
  ): Device {
    const created = "2026-10-01T00:00:00Z";
    const existing = options.modelId === undefined ? undefined : this.models.get(options.modelId);
    if (options.modelId !== undefined && existing === undefined) throw new Error("no such model");
    const model: DeviceModel = existing ?? {
      model_id: fakeUuid(),
      name: options.modelName ?? "power-meter",
      revision: 1,
      created_at: created,
      updated_at: created,
      points: (options.points ?? []).map((point, index) => ({
        key: point.key,
        function_code: 3,
        address: index * 2,
        data_type: point.data_type,
        description: point.description ?? null,
        ...(point.unit === undefined ? {} : { unit: point.unit }),
      })),
    };
    const device: Device = {
      device_id: fakeUuid(),
      name: options.name ?? "line-1-meter",
      hardware_id: options.hardwareId ?? "tg-0001",
      model_id: model.model_id,
      transport: options.transport ?? "modbus_tcp",
      host: options.host ?? "192.0.2.10",
      port: options.port ?? 502,
      unit_id: options.unitId ?? 1,
      poll_interval_ms: options.pollIntervalMs ?? 60_000,
      revision: 1,
      created_at: created,
      updated_at: created,
    };
    this.models.set(model.model_id, model);
    this.devices.set(device.device_id, device);
    return device;
  }

  /** A model without devices; returns it. */
  addModel(name: string): DeviceModel {
    const created = "2026-10-01T00:00:00Z";
    const model: DeviceModel = {
      model_id: fakeUuid(),
      name,
      revision: 1,
      created_at: created,
      updated_at: created,
      points: [],
    };
    this.models.set(model.model_id, model);
    return model;
  }

  /** Stores readings of one point. */
  addReadings(device: string, point: string, readings: readonly (readonly [number | string, Reading])[]): void {
    for (const [time, value] of readings) {
      this.readings.push({ device, point, time: typeof time === "string" ? Date.parse(time) : time, value });
    }
  }

  /** The next request to `endpoint` gets `response` instead of its answer. */
  failNext(endpoint: Endpoint, response: Response): void {
    this.failures.set(endpoint, [...(this.failures.get(endpoint) ?? []), response]);
  }

  /** The write endpoints and form reads of the model and device screens, through the real client. */
  assetsApi(): AssetsApi {
    return assetsApi(createApiClient({ fetch: (request) => this.fetch(request) }));
  }

  /** The device page's API, through the real client, against this server. */
  api(): DeviceApi {
    return deviceApi(createApiClient({ fetch: (request) => this.fetch(request) }));
  }

  /** The URLs of the series requests made so far. */
  seriesRequests(): URL[] {
    return this.requests.filter((url) => url.pathname.endsWith("/series"));
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    this.requests.push(url);
    const handled = await this.assets.handle(request, url);
    if (handled) return handled;
    if (request.method !== "GET") return error(405, "invalid_request", "error.invalid_request");
    const route = this.route(url.pathname);
    if (route === null) return error(404, "not_found", "error.not_found");
    const failure = this.failures.get(route.endpoint)?.shift();
    if (failure) return failure;
    switch (route.endpoint) {
      case "devices":
        return json(200, { devices: [...this.devices.values()].sort(byName) });
      case "models":
        return json(200, {
          models: [...this.models.values()]
            .sort(byName)
            .map(({ points, ...summary }) => ({ ...summary, point_count: points.length })),
        });
      case "device": {
        const device = this.devices.get(route.id);
        return device ? json(200, device) : error(404, "not_found", "error.not_found");
      }
      case "model": {
        const model = this.models.get(route.id);
        return model ? json(200, model) : error(404, "not_found", "error.not_found");
      }
      case "latest":
        return this.latest(route.id);
      case "series":
        return this.series(route.id, url.searchParams);
    }
  }

  private route(path: string): { endpoint: Endpoint; id: string } | null {
    const patterns: [Endpoint, RegExp][] = [
      ["devices", /^\/api\/assets\/v1\/devices()$/],
      ["models", /^\/api\/assets\/v1\/device-models()$/],
      ["device", /^\/api\/assets\/v1\/devices\/([^/]+)$/],
      ["model", /^\/api\/assets\/v1\/device-models\/([^/]+)$/],
      ["latest", /^\/api\/telemetry\/v1\/devices\/([^/]+)\/latest$/],
      ["series", /^\/api\/telemetry\/v1\/devices\/([^/]+)\/series$/],
    ];
    for (const [endpoint, pattern] of patterns) {
      const match = pattern.exec(path);
      if (match) return { endpoint, id: decodeURIComponent(match[1] as string) };
    }
    return null;
  }

  private latest(device: string): Response {
    const earliest = this.now() - this.rawPeriodMs;
    const newest = new Map<string, StoredReading>();
    for (const reading of this.readings) {
      if (reading.device !== device || reading.time < earliest) continue;
      const current = newest.get(reading.point);
      if (!current || reading.time >= current.time) newest.set(reading.point, reading);
    }
    const points = [...newest.values()]
      .sort((a, b) => (a.point < b.point ? -1 : a.point > b.point ? 1 : 0))
      .map((reading) => ({ point: reading.point, time: new Date(reading.time).toISOString(), value: reading.value }));
    return json(200, { device_id: device, earliest: new Date(earliest).toISOString(), points });
  }

  private series(device: string, query: URLSearchParams): Response {
    const point = query.get("point");
    const tz = query.get("tz");
    const from = Date.parse(query.get("from") ?? "");
    const to = Date.parse(query.get("to") ?? "");
    if (point === null || tz === null || !Number.isFinite(from) || !Number.isFinite(to)) {
      return error(400, "invalid_request", "telemetry.query_invalid");
    }
    if (from >= to) return error(422, "validation_failed", "telemetry.range_invalid", { parameter: "from" });
    // The aggregates' period starts on a whole hour, as the service's cut does.
    const earliest = Math.ceil((this.now() - this.aggregatedPeriodMs) / 3_600_000) * 3_600_000;
    if (to <= earliest) {
      return error(403, "quota_exceeded", "telemetry.outside_queryable_period", {
        earliest: new Date(earliest).toISOString(),
      });
    }
    const start = Math.max(from, earliest);
    const interval =
      AGGREGATED.find((candidate) => Math.ceil((to - start) / (INTERVAL_MS[candidate] as number)) <= MAX_BUCKETS) ??
      "1d";
    const length = INTERVAL_MS[interval] as number;
    const bucketOf = (time: number) => {
      // Minute buckets count from the epoch; hour and day buckets from local hours and midnights.
      const offset = length >= 3_600_000 ? offsetAt(time, tz) : 0;
      return Math.floor((time + offset) / length) * length - offset;
    };
    const buckets = new Map<number, StoredReading[]>();
    for (const reading of this.readings) {
      if (reading.device !== device || reading.point !== point) continue;
      const bucket = bucketOf(reading.time);
      if (bucket < start || bucket >= to) continue;
      buckets.set(bucket, [...(buckets.get(bucket) ?? []), reading]);
    }
    const points: SeriesPoint[] = [...buckets.entries()]
      .sort(([a], [b]) => a - b)
      .map(([bucket, readings]) => {
        const sorted = [...readings].sort((a, b) => a.time - b.time);
        const numbers = sorted
          .map(({ value }) =>
            value.type === "bool" ? (value.value ? 1 : 0) : value.type === "string" ? null : value.value,
          )
          .filter((value): value is number => value !== null && Number.isFinite(value));
        const last = sorted[sorted.length - 1] as StoredReading;
        return {
          time: new Date(bucket).toISOString(),
          count: sorted.length,
          last: last.value,
          min: numbers.length > 0 ? Math.min(...numbers) : null,
          max: numbers.length > 0 ? Math.max(...numbers) : null,
          avg: numbers.length > 0 ? numbers.reduce((sum, value) => sum + value, 0) / numbers.length : null,
        };
      });
    const body: Series = {
      device_id: device,
      point,
      tz,
      interval,
      from: new Date(start).toISOString(),
      to: new Date(to).toISOString(),
      truncated: from < earliest,
      earliest: new Date(earliest).toISOString(),
      points,
    };
    return json(200, body);
  }
}

export { error as fakeError };

/**
 * One reading every 10 minutes over the last three days, from `value(hours ago)`. `newest` stops
 * the readings that many hours ago (a device that went quiet); `outage` leaves out the readings
 * between two hours-ago marks (a hole the history chart draws as a gap).
 */
function previewReadings(
  now: number,
  value: (hoursAgo: number) => Reading,
  options: { newest?: number; outage?: [from: number, to: number] } = {},
): [number, Reading][] {
  const step = 10 * 60_000;
  const readings: [number, Reading][] = [];
  for (let time = now - 3 * DAY; time <= now; time += step) {
    const hoursAgo = (now - time) / 3_600_000;
    if (options.newest !== undefined && hoursAgo < options.newest) continue;
    if (options.outage && hoursAgo < options.outage[0] && hoursAgo > options.outage[1]) continue;
    readings.push([time, value(hoursAgo)]);
  }
  return readings;
}

/**
 * The UI preview's devices: a temperature and humidity transmitter, a power meter and a
 * compressor, on one gateway, with three days of readings up to now and a model without devices.
 * `empty` has neither devices nor models.
 */
export const preview: HttpPreviewDeclaration = {
  name: "devices",
  http: {
    // The gateway list and the plan's limits are what the device form chooses from (assets-rules.ts).
    prefixes: ["/api/assets/v1", "/api/telemetry/v1", "/api/gateway-ops/v1/gateways", "/api/identity/v1/entitlements"],
    create: (seed) => {
      const server = new FakeDeviceServer();
      // An empty tenant has bound its first gateway and nothing else.
      if (seed === "empty") server.assets.gateways.splice(1);
      // The same four gateways as the gateway page's preview (fake-gateways.ts), so the device form and
      // the gateway list name the same ones.
      if (seed === "normal") {
        server.assets.gateways.push(
          { hardware_id: "tg-0003", presence: "online" },
          { hardware_id: "tg-0004", presence: "unknown" },
        );
      }
      if (seed === "normal") {
        const now = Date.now();
        const float = (value: number): Reading => ({ type: "float", value: Math.round(value * 10) / 10 });
        const room = server.addDevice({
          name: "cold-room-1",
          modelName: "temp-humidity-transmitter",
          points: [
            { key: "temperature", data_type: "f32", description: "Air temperature (°C)" },
            { key: "humidity", data_type: "f32", description: "Relative humidity (%)" },
          ],
          host: "192.0.2.21",
          unitId: 1,
        });
        server.addReadings(
          room.device_id,
          "temperature",
          previewReadings(now, (h) => float(4 + 1.2 * Math.sin(h / 3))),
        );
        server.addReadings(
          room.device_id,
          "humidity",
          previewReadings(now, (h) => float(72 + 6 * Math.cos(h / 5))),
        );
        const meter = server.addDevice({
          name: "line-2-meter",
          modelName: "power-meter",
          points: [
            { key: "power_kw", data_type: "f32", description: "Active power (kW)" },
            { key: "energy_kwh", data_type: "u32", description: "Energy since installation (kWh)" },
          ],
          host: "192.0.2.22",
          unitId: 2,
        });
        server.addReadings(
          meter.device_id,
          "power_kw",
          previewReadings(now, (h) => float(38 + 14 * Math.sin(h / 4))),
        );
        server.addReadings(
          meter.device_id,
          "energy_kwh",
          previewReadings(now, (h) => ({ type: "int", value: Math.round(182_400 - h * 38) })),
        );
        const compressor = server.addDevice({
          name: "compressor-1",
          modelName: "compressor",
          points: [
            { key: "running", data_type: "bool" },
            { key: "pressure_bar", data_type: "f32", description: "Outlet pressure (bar)" },
          ],
          host: "192.0.2.23",
          unitId: 3,
          pollIntervalMs: 30_000,
        });
        server.addReadings(
          compressor.device_id,
          "running",
          previewReadings(now, (h) => ({ type: "bool", value: Math.floor(h) % 5 !== 0 })),
        );
        server.addReadings(
          compressor.device_id,
          "pressure_bar",
          previewReadings(now, (h) => float(7.5 + 0.4 * Math.sin(h))),
        );
        // The states of the device page that the three devices above do not show: a device whose
        // data stopped (stalled, with a text point), a history with a hole in it, and a device
        // that never stored a reading.
        const boiler = server.addDevice({
          name: "boiler-1",
          modelName: "boiler-controller",
          points: [
            { key: "water_temp", data_type: "f32", description: "Water temperature (°C)" },
            { key: "state", data_type: "string", description: "Controller state" },
          ],
          host: "192.0.2.24",
          unitId: 4,
        });
        const quiet = { newest: 0.75 };
        server.addReadings(
          boiler.device_id,
          "water_temp",
          previewReadings(now, (h) => float(78 + 3 * Math.sin(h / 2)), quiet),
        );
        server.addReadings(
          boiler.device_id,
          "state",
          previewReadings(
            now,
            (h) => ({ type: "string", value: Math.floor(h / 6) % 2 === 0 ? "heating" : "holding" }),
            quiet,
          ),
        );
        const chiller = server.addDevice({
          name: "chiller-2",
          modelName: "chiller",
          points: [{ key: "supply_temp", data_type: "f32", description: "Supply water temperature (°C)" }],
          host: "192.0.2.25",
          unitId: 5,
        });
        server.addReadings(
          chiller.device_id,
          "supply_temp",
          previewReadings(now, (h) => float(7 + 0.8 * Math.sin(h / 2)), { outage: [20, 14] }),
        );
        server.addDevice({
          name: "line-4-meter",
          modelId: meter.model_id,
          host: "192.0.2.26",
          unitId: 6,
        });
        server.addModel("flow-meter");
      }
      return { fetch: (request) => server.fetch(request) };
    },
  },
};
