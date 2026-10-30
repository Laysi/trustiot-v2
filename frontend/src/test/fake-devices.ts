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

type Endpoint = "device" | "model" | "latest" | "series";

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

let ids = 0;
/** A fresh lowercase UUID (the API's form), unique within the test run. */
export function fakeUuid(): string {
  ids += 1;
  return `00000000-0000-4000-8000-${ids.toString(16).padStart(12, "0")}`;
}

export class FakeDeviceServer {
  readonly devices = new Map<string, Device>();
  readonly models = new Map<string, DeviceModel>();
  readonly readings: StoredReading[] = [];
  /** Every request's URL, in order. */
  readonly requests: URL[] = [];
  /** The server's clock. */
  now: () => number = () => Date.now();
  /** How far back the raw readings (latest values) and the aggregates (series) can be queried. */
  rawPeriodMs = 7 * DAY;
  aggregatedPeriodMs = 92 * DAY;
  private readonly failures = new Map<Endpoint, Response[]>();

  /** A model with these points, and a device of it; returns the device. */
  addDevice(options: {
    name?: string;
    modelName?: string;
    points: { key: string; data_type: DeviceModel["points"][number]["data_type"]; description?: string }[];
    hardwareId?: string;
  }): Device {
    const created = "2026-10-01T00:00:00Z";
    const model: DeviceModel = {
      model_id: fakeUuid(),
      name: options.modelName ?? "power-meter",
      revision: 1,
      created_at: created,
      updated_at: created,
      points: options.points.map((point, index) => ({
        key: point.key,
        function_code: 3,
        address: index * 2,
        data_type: point.data_type,
        description: point.description ?? null,
      })),
    };
    const device: Device = {
      device_id: fakeUuid(),
      name: options.name ?? "line-1-meter",
      hardware_id: options.hardwareId ?? "tg-0001",
      model_id: model.model_id,
      transport: "modbus_tcp",
      host: "192.0.2.10",
      port: 502,
      unit_id: 1,
      poll_interval_ms: 60_000,
      revision: 1,
      created_at: created,
      updated_at: created,
    };
    this.models.set(model.model_id, model);
    this.devices.set(device.device_id, device);
    return device;
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
    if (request.method !== "GET") return error(405, "invalid_request", "error.invalid_request");
    const route = this.route(url.pathname);
    if (route === null) return error(404, "not_found", "error.not_found");
    const failure = this.failures.get(route.endpoint)?.shift();
    if (failure) return failure;
    switch (route.endpoint) {
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
