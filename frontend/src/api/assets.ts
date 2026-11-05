import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import type { ApiClient } from "./client.ts";
import { pinnable, unwrap } from "./client.ts";
import { type Device, DeviceModelPointSchema, DeviceModelSchema, DeviceSchema } from "./devices.ts";
import type { components } from "./schema.gen.ts";

type Spec = components["schemas"];

// The writes of the model and device screens: the assets service's create, update, delete, import
// and export (services/assets), plus the two reads a form needs that the device pages do not:
// the tenant's gateways (gateway-ops) and its plan's entitlements (identity). Each body is bound
// to the generated type, so a change in api/openapi.json fails `typecheck` (lib/type-binding.ts).

/** What `GET .../export` downloads and `POST .../import` uploads: no IDs, revisions or times. */
const DeviceModelDocumentSchema = z.object({
  format: z.string(),
  format_version: z.number().int(),
  name: z.string(),
  points: z.array(DeviceModelPointSchema),
});

/** The `format` every exported document carries. */
export const MODEL_DOCUMENT_FORMAT = "trustiot.device_model";
export const MODEL_DOCUMENT_VERSION = 1;

const PresenceSchema = z.enum(["online", "offline", "unknown"]);
/** The part of a gateway the device form needs: which gateway, and whether it is connected. */
const BoundGatewaySchema = z.object({ hardware_id: z.string(), presence: PresenceSchema });
const GatewayListSchema = z.object({ gateways: z.array(BoundGatewaySchema) });

const PlanSchema = z.enum(["starter", "business", "enterprise", "enterprise_on_prem"]);
/** The plan and the one entitlement the device form uses: the shortest polling interval. */
const EntitlementsSchema = z.object({
  plan: PlanSchema,
  entitlements: z.object({ min_sampling_interval_ms: z.number().int() }),
});

export type AssetsBodiesMatchSpec = Assert<
  AllTrue<
    [
      Equal<z.output<typeof DeviceModelDocumentSchema>, Spec["DeviceModelDocument"]>,
      Equal<z.output<typeof PresenceSchema>, Spec["Presence"]>,
      Equal<z.output<typeof PlanSchema>, Spec["Plan"]>,
      // Only the members the form reads are kept, so these are checked by assignability.
      Spec["Gateway"] extends z.output<typeof BoundGatewaySchema> ? true : false,
      Spec["CurrentEntitlements"] extends z.input<typeof EntitlementsSchema> ? true : false,
    ]
  >
>;

export type DeviceModelDocument = z.output<typeof DeviceModelDocumentSchema>;
export type BoundGateway = z.output<typeof BoundGatewaySchema>;
export type Plan = z.output<typeof PlanSchema>;
export type CreateDeviceInput = Spec["CreateDevice"];
export type UpdateDeviceInput = Spec["UpdateDevice"];
export type CreateDeviceModelInput = Spec["CreateDeviceModel"];
export type UpdateDeviceModelInput = Spec["UpdateDeviceModel"];
export type DeviceModelEntity = z.output<typeof DeviceModelSchema>;

/** The tenant's plan and the shortest polling interval it allows a device. */
export interface PlanLimits {
  plan: Plan;
  minPollIntervalMs: number;
}

/** The endpoints the model and device screens write, and the reads their forms need. */
export interface AssetsApi {
  createDevice(input: CreateDeviceInput): Promise<Device>;
  /** Replaces the device's settings; `input.revision` is the one the form read. */
  updateDevice(deviceId: string, input: UpdateDeviceInput): Promise<Device>;
  deleteDevice(deviceId: string): Promise<void>;
  createDeviceModel(input: CreateDeviceModelInput): Promise<DeviceModelEntity>;
  /** Replaces the model's data points; `input.revision` is the one the form read. */
  updateDeviceModel(modelId: string, input: UpdateDeviceModelInput): Promise<DeviceModelEntity>;
  deleteDeviceModel(modelId: string): Promise<void>;
  importDeviceModel(document: DeviceModelDocument): Promise<DeviceModelEntity>;
  exportDeviceModel(modelId: string): Promise<DeviceModelDocument>;
  /** The gateways bound to the tenant, by hardware ID. */
  listGateways(signal?: AbortSignal): Promise<BoundGateway[]>;
  getPlanLimits(signal?: AbortSignal): Promise<PlanLimits>;
}

/** The assets write endpoints and the form reads, through the typed client (`pinnable`: a page binds its own copy). */
export function assetsApi(client: ApiClient): AssetsApi {
  return pinnable(client, (client) => ({
    createDevice: (input) => unwrap(DeviceSchema, client.POST("/api/assets/v1/devices", { body: input })),
    updateDevice: (deviceId, input) =>
      unwrap(
        DeviceSchema,
        client.PUT("/api/assets/v1/devices/{device_id}", { params: { path: { device_id: deviceId } }, body: input }),
      ),
    deleteDevice: (deviceId) =>
      unwrap(
        z.undefined(),
        client.DELETE("/api/assets/v1/devices/{device_id}", { params: { path: { device_id: deviceId } } }),
      ),
    createDeviceModel: (input) =>
      unwrap(DeviceModelSchema, client.POST("/api/assets/v1/device-models", { body: input })),
    updateDeviceModel: (modelId, input) =>
      unwrap(
        DeviceModelSchema,
        client.PUT("/api/assets/v1/device-models/{model_id}", {
          params: { path: { model_id: modelId } },
          body: input,
        }),
      ),
    deleteDeviceModel: (modelId) =>
      unwrap(
        z.undefined(),
        client.DELETE("/api/assets/v1/device-models/{model_id}", { params: { path: { model_id: modelId } } }),
      ),
    importDeviceModel: (document) =>
      unwrap(DeviceModelSchema, client.POST("/api/assets/v1/device-models/import", { body: document })),
    exportDeviceModel: (modelId) =>
      unwrap(
        DeviceModelDocumentSchema,
        client.GET("/api/assets/v1/device-models/{model_id}/export", { params: { path: { model_id: modelId } } }),
      ),
    listGateways: async (signal) =>
      (await unwrap(GatewayListSchema, client.GET("/api/gateway-ops/v1/gateways", signal ? { signal } : {}))).gateways,
    getPlanLimits: async (signal) => {
      const body = await unwrap(
        EntitlementsSchema,
        client.GET("/api/identity/v1/entitlements", signal ? { signal } : {}),
      );
      return { plan: body.plan, minPollIntervalMs: body.entitlements.min_sampling_interval_ms };
    },
  }));
}
