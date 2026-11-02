import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import type { ApiClient } from "./client.ts";
import { pinnable, unwrap } from "./client.ts";
import type { components } from "./schema.gen.ts";

type Spec = components["schemas"];

// The device page's bodies: the assets service's device and model (services/assets) and the
// telemetry-query service's latest values and series (services/telemetry-query/README.md, 「API」).
// Each is bound to the generated type, so a change in api/openapi.json fails `typecheck`
// (lib/type-binding.ts).
const DeviceSchema = z.object({
  device_id: z.string(),
  name: z.string(),
  hardware_id: z.string(),
  model_id: z.string(),
  transport: z.enum(["modbus_tcp", "rtu_over_tcp"]),
  host: z.string(),
  port: z.number().int(),
  unit_id: z.number().int(),
  poll_interval_ms: z.number().int(),
  revision: z.number().int(),
  created_at: z.string(),
  updated_at: z.string(),
});

const DataTypeSchema = z.enum(["bool", "u16", "i16", "u32", "i32", "u64", "i64", "f32", "f64", "string"]);
const DeviceModelPointSchema = z.object({
  key: z.string(),
  function_code: z.number().int(),
  address: z.number().int(),
  data_type: DataTypeSchema,
  byte_order: z.enum(["abcd", "badc", "cdab", "dcba"]).optional(),
  description: z.string().nullable().optional(),
  length: z.number().int().nullable().optional(),
  scale: z.number().nullable().optional(),
  offset: z.number().nullable().optional(),
});
const DeviceModelSchema = z.object({
  model_id: z.string(),
  name: z.string(),
  revision: z.number().int(),
  created_at: z.string(),
  updated_at: z.string(),
  points: z.array(DeviceModelPointSchema),
});

/** One stored reading, as its type stored it; a float is `null` for NaN or an infinity. */
const ReadingSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("float"), value: z.number().nullable() }),
  z.object({ type: z.literal("int"), value: z.number() }),
  z.object({ type: z.literal("bool"), value: z.boolean() }),
  z.object({ type: z.literal("string"), value: z.string() }),
]);
const LatestPointSchema = z.object({ point: z.string(), time: z.string(), value: ReadingSchema });
const LatestSchema = z.object({ device_id: z.string(), earliest: z.string(), points: z.array(LatestPointSchema) });

export const INTERVALS = ["raw", "1m", "5m", "15m", "1h", "3h", "6h", "12h", "1d"] as const;
const IntervalSchema = z.enum(INTERVALS);
const SeriesPointSchema = z.object({
  time: z.string(),
  count: z.number().int(),
  last: ReadingSchema,
  min: z.number().nullable().optional(),
  max: z.number().nullable().optional(),
  avg: z.number().nullable().optional(),
});
const SeriesSchema = z.object({
  device_id: z.string(),
  point: z.string(),
  tz: z.string(),
  interval: IntervalSchema,
  from: z.string(),
  to: z.string(),
  truncated: z.boolean(),
  earliest: z.string(),
  points: z.array(SeriesPointSchema),
});

export type DeviceBodiesMatchSpec = Assert<
  AllTrue<
    [
      Equal<z.output<typeof DeviceSchema>, Spec["Device"]>,
      Equal<z.output<typeof DeviceModelPointSchema>, Spec["DeviceModelPoint"]>,
      Equal<z.output<typeof DeviceModelSchema>, Spec["DeviceModel"]>,
      Equal<z.output<typeof ReadingSchema>, Spec["Reading"]>,
      Equal<z.output<typeof LatestPointSchema>, Spec["LatestPoint"]>,
      Equal<z.output<typeof LatestSchema>, Spec["Latest"]>,
      Equal<z.output<typeof IntervalSchema>, Spec["Interval"]>,
      Equal<z.output<typeof SeriesPointSchema>, Spec["SeriesPoint"]>,
      Equal<z.output<typeof SeriesSchema>, Spec["Series"]>,
    ]
  >
>;

export type Device = z.output<typeof DeviceSchema>;
export type DeviceModel = z.output<typeof DeviceModelSchema>;
export type DeviceModelPoint = z.output<typeof DeviceModelPointSchema>;
export type DataType = z.output<typeof DataTypeSchema>;
export type Reading = z.output<typeof ReadingSchema>;
export type Latest = z.output<typeof LatestSchema>;
export type Interval = z.output<typeof IntervalSchema>;
export type SeriesPoint = z.output<typeof SeriesPointSchema>;
export type Series = z.output<typeof SeriesSchema>;

/** What the device page asks for one data point's history. */
export interface SeriesRequest {
  deviceId: string;
  point: string;
  /** Inclusive start and exclusive end, as instants. */
  from: Date;
  to: Date;
  /** The IANA time zone hour and day buckets follow: the user's. */
  tz: string;
}

/** The endpoints the device page reads: one device, its model, its latest values and a series. */
export interface DeviceApi {
  getDevice(deviceId: string, signal?: AbortSignal): Promise<Device>;
  getDeviceModel(modelId: string, signal?: AbortSignal): Promise<DeviceModel>;
  getLatest(deviceId: string, signal?: AbortSignal): Promise<Latest>;
  /** The interval is left to the server: the finest one that gives at most 1500 buckets. */
  getSeries(request: SeriesRequest, signal?: AbortSignal): Promise<Series>;
}

/** The device and telemetry endpoints, through the typed client (`pinnable`: a page binds its own copy). */
export function deviceApi(client: ApiClient): DeviceApi {
  return pinnable(client, (client) => ({
    getDevice: (deviceId, signal) =>
      unwrap(
        DeviceSchema,
        client.GET("/api/assets/v1/devices/{device_id}", {
          params: { path: { device_id: deviceId } },
          ...(signal ? { signal } : {}),
        }),
      ),
    getDeviceModel: (modelId, signal) =>
      unwrap(
        DeviceModelSchema,
        client.GET("/api/assets/v1/device-models/{model_id}", {
          params: { path: { model_id: modelId } },
          ...(signal ? { signal } : {}),
        }),
      ),
    getLatest: (deviceId, signal) =>
      unwrap(
        LatestSchema,
        client.GET("/api/telemetry/v1/devices/{device_id}/latest", {
          params: { path: { device_id: deviceId } },
          ...(signal ? { signal } : {}),
        }),
      ),
    getSeries: ({ deviceId, point, from, to, tz }, signal) =>
      unwrap(
        SeriesSchema,
        client.GET("/api/telemetry/v1/devices/{device_id}/series", {
          // UTC (`Z`), so no `+` offset needs percent-encoding.
          params: {
            path: { device_id: deviceId },
            query: { point, from: from.toISOString(), to: to.toISOString(), tz },
          },
          ...(signal ? { signal } : {}),
        }),
      ),
  }));
}
