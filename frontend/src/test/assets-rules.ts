import type { BoundGateway, PlanLimits } from "../api/assets.ts";
import type { Device, DeviceModel, DeviceModelPoint } from "../api/devices.ts";

/**
 * The assets service's writes (create, update, delete, import and export of devices and models),
 * the tenant's gateways and its plan limits, for `FakeDeviceServer` (fake-devices.ts, which owns
 * the assets paths of the UI preview, so the reads and the writes act on one set of devices and
 * models). The rules it keeps are the ones a screen shows: a name is unique in the tenant and in
 * the naming format, an update must send the revision it read (409 `*.revision_mismatch` with
 * `details.current`), a device on an unbound gateway cannot be updated (409
 * `device.binding_ended`), a model that a device uses cannot be deleted (409
 * `device_model.in_use`), a device needs a bound gateway and an existing model. Every other
 * refusal (a field error from the real service's validation, a plan limit, a 503) is put in place
 * by `failNext`.
 *
 * Every write is recorded in `writes`; `failNext` answers the next request to an operation with
 * the given response instead of its answer.
 */

/** What the assets rules need of the server they belong to. */
export interface AssetsHost {
  devices: Map<string, Device>;
  models: Map<string, DeviceModel>;
  now(): number;
  newId(): string;
  addModel(name: string): DeviceModel;
}

export type WriteOperation =
  | "createDevice"
  | "updateDevice"
  | "deleteDevice"
  | "createModel"
  | "updateModel"
  | "deleteModel"
  | "importModel"
  | "exportModel"
  | "gateways"
  | "planLimits";

/** A request a screen made, as the service saw it. */
export interface RecordedWrite {
  operation: WriteOperation;
  method: string;
  path: string;
  body: unknown;
}

const NAME = /^[a-z][a-z0-9]*(-[a-z0-9]+)*$/;

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

function refused(status: number, code: string, messageKey: string, details?: Record<string, string | number>) {
  return json(status, { code, message_key: messageKey, ...(details ? { details } : {}) });
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** A 422 for one field, with the message key the service uses. */
export function fieldRefusal(field: string, messageKey: string, details?: Record<string, string | number>): Response {
  return json(422, {
    code: "validation_failed",
    message_key: "error.validation_failed",
    field_errors: [{ field, message_key: messageKey, ...(details ? { details } : {}) }],
  });
}

export class FakeAssets {
  /** The gateways bound to the tenant. */
  readonly gateways: BoundGateway[] = [
    { hardware_id: "tg-0001", presence: "online" },
    { hardware_id: "tg-0002", presence: "offline" },
  ];
  /** The tenant's plan and its shortest polling interval. */
  planLimits: PlanLimits = { plan: "starter", minPollIntervalMs: 60_000 };
  /** Devices whose gateway was unbound: an update answers 409 `device.binding_ended`. */
  readonly endedDevices = new Set<string>();
  /** Every write and form read, in order. */
  readonly writes: RecordedWrite[] = [];
  private readonly writeFailures = new Map<WriteOperation, Response[]>();

  constructor(private readonly host: AssetsHost) {}

  /** The next request to `operation` gets `response` instead of its answer. */
  failNext(operation: WriteOperation, response: Response): void {
    this.writeFailures.set(operation, [...(this.writeFailures.get(operation) ?? []), response]);
  }

  /** A model with these points; returns it. */
  addModelWithPoints(name: string, points: readonly DeviceModelPoint[]): DeviceModel {
    const model = this.host.addModel(name);
    model.points = points.map((point) => ({ ...point }));
    return model;
  }

  /** Another change made by someone else: the device's revision is one higher than the form read. */
  bumpDevice(deviceId: string, changes: Partial<Pick<Device, "host" | "port" | "unit_id" | "poll_interval_ms">>): void {
    const device = this.host.devices.get(deviceId);
    if (!device) throw new Error("no such device");
    this.host.devices.set(deviceId, { ...device, ...changes, revision: device.revision + 1 });
  }

  /** Another change made by someone else to a model's points. */
  bumpModel(modelId: string, points: readonly DeviceModelPoint[]): void {
    const model = this.host.models.get(modelId);
    if (!model) throw new Error("no such model");
    this.host.models.set(modelId, {
      ...model,
      points: points.map((point) => ({ ...point })),
      revision: model.revision + 1,
    });
  }

  /** The answer to a write or a form read; `null` for a request that is not one (the server answers it). */
  async handle(request: Request, url: URL): Promise<Response | null> {
    const path = url.pathname;
    const method = request.method;
    const take = (operation: WriteOperation, body: unknown): Response | null => {
      this.writes.push({ operation, method, path, body });
      return this.writeFailures.get(operation)?.shift() ?? null;
    };
    const body = async (): Promise<unknown> => {
      try {
        return await request.json();
      } catch {
        return undefined;
      }
    };

    if (method === "GET" && path === "/api/gateway-ops/v1/gateways") {
      return take("gateways", undefined) ?? json(200, { gateways: this.gateways });
    }
    if (method === "GET" && path === "/api/identity/v1/entitlements") {
      return (
        take("planLimits", undefined) ??
        json(200, {
          tenant_id: "00000000-0000-4000-8000-0000000000aa",
          plan: this.planLimits.plan,
          entitlements_version: 1,
          entitlements: { min_sampling_interval_ms: this.planLimits.minPollIntervalMs },
        })
      );
    }

    let match = /^\/api\/assets\/v1\/device-models\/([^/]+)\/export$/.exec(path);
    if (method === "GET" && match) {
      const refusal = take("exportModel", undefined);
      if (refusal) return refusal;
      const model = this.host.models.get(decodeURIComponent(match[1] as string));
      if (!model) return refused(404, "not_found", "error.not_found");
      return json(200, {
        format: "trustiot.device_model",
        format_version: 1,
        name: model.name,
        points: model.points,
      });
    }

    if (method === "POST" && path === "/api/assets/v1/devices") {
      const sent = await body();
      return take("createDevice", sent) ?? this.createDevice(sent);
    }
    match = /^\/api\/assets\/v1\/devices\/([^/]+)$/.exec(path);
    if (match && method === "PUT") {
      const sent = await body();
      return take("updateDevice", sent) ?? this.updateDevice(decodeURIComponent(match[1] as string), sent);
    }
    if (match && method === "DELETE") {
      const id = decodeURIComponent(match[1] as string);
      return (
        take("deleteDevice", undefined) ??
        (this.host.devices.delete(id)
          ? new Response(null, { status: 204 })
          : refused(404, "not_found", "error.not_found"))
      );
    }

    if (method === "POST" && path === "/api/assets/v1/device-models") {
      const sent = await body();
      return take("createModel", sent) ?? this.createModel(sent, false);
    }
    if (method === "POST" && path === "/api/assets/v1/device-models/import") {
      const sent = await body();
      return take("importModel", sent) ?? this.createModel(sent, true);
    }
    match = /^\/api\/assets\/v1\/device-models\/([^/]+)$/.exec(path);
    if (match && method === "PUT") {
      const sent = await body();
      return take("updateModel", sent) ?? this.updateModel(decodeURIComponent(match[1] as string), sent);
    }
    if (match && method === "DELETE") {
      const id = decodeURIComponent(match[1] as string);
      const refusal = take("deleteModel", undefined);
      if (refusal) return refusal;
      if (!this.host.models.has(id)) return refused(404, "not_found", "error.not_found");
      const using = [...this.host.devices.values()].filter((device) => device.model_id === id).length;
      if (using > 0) return refused(409, "conflict", "device_model.in_use", { devices: using });
      this.host.models.delete(id);
      return new Response(null, { status: 204 });
    }
    return null;
  }

  private nameTaken(name: string, among: Iterable<{ name: string }>): boolean {
    for (const item of among) if (item.name === name) return true;
    return false;
  }

  private createDevice(sent: unknown): Response {
    if (!isRecord(sent)) return refused(400, "invalid_request", "device.body_invalid");
    const { name, hardware_id: hardwareId, model_id: modelId, transport, host, port } = sent;
    if (typeof name !== "string" || !NAME.test(name) || name.length > 40) {
      return fieldRefusal("/name", "device.name_invalid", { max: 40 });
    }
    if (typeof hardwareId !== "string" || !this.gateways.some((gateway) => gateway.hardware_id === hardwareId)) {
      return fieldRefusal("/hardware_id", "device.gateway_not_bound");
    }
    if (typeof modelId !== "string" || !this.host.models.has(modelId)) {
      return fieldRefusal("/model_id", "device.model_not_found");
    }
    if (this.nameTaken(name, this.host.devices.values())) {
      return refused(409, "conflict", "device.name_taken");
    }
    const now = new Date(this.host.now()).toISOString();
    const device: Device = {
      device_id: this.host.newId(),
      name,
      hardware_id: hardwareId,
      model_id: modelId,
      transport: transport === "rtu_over_tcp" ? "rtu_over_tcp" : "modbus_tcp",
      host: typeof host === "string" ? host : "",
      port: typeof port === "number" ? port : 502,
      unit_id: typeof sent.unit_id === "number" ? sent.unit_id : 1,
      poll_interval_ms: typeof sent.poll_interval_ms === "number" ? sent.poll_interval_ms : 60_000,
      revision: 1,
      created_at: now,
      updated_at: now,
    };
    this.host.devices.set(device.device_id, device);
    return json(201, device);
  }

  private updateDevice(id: string, sent: unknown): Response {
    const device = this.host.devices.get(id);
    if (!device) return refused(404, "not_found", "error.not_found");
    if (!isRecord(sent)) return refused(400, "invalid_request", "device.body_invalid");
    if (this.endedDevices.has(id)) return refused(409, "conflict", "device.binding_ended");
    if (sent.revision !== device.revision) {
      return refused(409, "conflict", "device.revision_mismatch", { current: device.revision });
    }
    const updated: Device = {
      ...device,
      transport: sent.transport === "rtu_over_tcp" ? "rtu_over_tcp" : "modbus_tcp",
      host: typeof sent.host === "string" ? sent.host : device.host,
      port: typeof sent.port === "number" ? sent.port : device.port,
      // A field left out takes its default: the update replaces every setting.
      unit_id: typeof sent.unit_id === "number" ? sent.unit_id : 1,
      poll_interval_ms: typeof sent.poll_interval_ms === "number" ? sent.poll_interval_ms : 60_000,
      revision: device.revision + 1,
      updated_at: new Date(this.host.now()).toISOString(),
    };
    this.host.devices.set(id, updated);
    return json(200, updated);
  }

  private createModel(sent: unknown, document: boolean): Response {
    if (!isRecord(sent)) return refused(400, "invalid_request", "device_model.body_invalid");
    if (document && (sent.format !== "trustiot.device_model" || typeof sent.format !== "string")) {
      return fieldRefusal("/format", "device_model.format_unsupported");
    }
    if (document && sent.format_version !== 1) {
      return fieldRefusal("/format_version", "device_model.format_version_unsupported");
    }
    const { name, points } = sent;
    if (typeof name !== "string" || !NAME.test(name) || name.length > 40) {
      return fieldRefusal("/name", "device_model.name_invalid", { max: 40 });
    }
    if (!Array.isArray(points)) return fieldRefusal("/points", "device_model.points_invalid");
    if (this.nameTaken(name, this.host.models.values())) {
      return refused(409, "conflict", "device_model.name_taken");
    }
    const now = new Date(this.host.now()).toISOString();
    const model: DeviceModel = {
      model_id: this.host.newId(),
      name,
      revision: 1,
      created_at: now,
      updated_at: now,
      points: points as DeviceModelPoint[],
    };
    this.host.models.set(model.model_id, model);
    return json(201, model);
  }

  private updateModel(id: string, sent: unknown): Response {
    const model = this.host.models.get(id);
    if (!model) return refused(404, "not_found", "error.not_found");
    if (!isRecord(sent)) return refused(400, "invalid_request", "device_model.body_invalid");
    if (sent.revision !== model.revision) {
      return refused(409, "conflict", "device_model.revision_mismatch", { current: model.revision });
    }
    if (!Array.isArray(sent.points)) return fieldRefusal("/points", "device_model.points_invalid");
    const updated: DeviceModel = {
      ...model,
      points: sent.points as DeviceModelPoint[],
      revision: model.revision + 1,
      updated_at: new Date(this.host.now()).toISOString(),
    };
    this.host.models.set(id, updated);
    return json(200, updated);
  }
}
