import { createElement, type ReactNode } from "react";

import { ApiError } from "../api/errors.ts";
import {
  type AnomalyRow,
  type DeviceHealthRow,
  type HomeOverviewApi,
  HomeOverviewApiProvider,
  type NewAnomalies,
  type StatusSummary,
  type TopUsage,
} from "../api/pending/home-overview.ts";

/**
 * The generic home's endpoints (`api/pending/home-overview.ts`) in memory. The summary, the top
 * usage and the anomalies are computed from the device rows and the anomaly list, so the figures
 * always agree with the table. Every call is recorded; `failNext` makes the next call to an
 * operation fail, so each block's error state can be shown on its own.
 */

export type OverviewOperation = "summary" | "usage" | "anomalies" | "devices";

export function unavailable(): ApiError {
  return new ApiError(503, { code: "unavailable", message_key: "error.unavailable" });
}

/** The demo tenant of the design (docs/design/home-ai/10-fallback-generic-home.html), as fictional as it is. */
export function demoDevices(): DeviceHealthRow[] {
  const row = (
    id: string,
    name: string,
    model: string,
    gateway: string,
    status: DeviceHealthRow["status"],
    kwh: number | null,
    last: string | null,
  ): DeviceHealthRow => ({
    device_id: `00000000-0000-4000-8000-0000000000${id}`,
    name,
    model_name: model,
    gateway,
    status,
    usage_today_kwh: kwh,
    last_report_at: last,
  });
  return [
    row("01", "Main panel P1", "Power meter", "GW-01", "normal", 412, "2026-11-04T10:24:00Z"),
    row("02", "Compressor A1", "Screw 37 kW", "GW-01", "normal", 268, "2026-11-04T10:24:00Z"),
    row("03", "Boiler B1", "Gas boiler", "GW-02", "abnormal", 184, "2026-11-04T10:24:00Z"),
    row("04", "Injection press M3", "180 t", "GW-02", "normal", 151, "2026-11-04T10:23:00Z"),
    row("05", "Cooling tower C1", "Inverter pump", "GW-02", "normal", 97, "2026-11-04T10:24:00Z"),
    row("06", "Compressor A2", "Screw 37 kW", "GW-01", "offline", 0, "2026-11-04T09:02:00Z"),
  ];
}

export function demoAnomalies(devices: readonly DeviceHealthRow[]): AnomalyRow[] {
  const by = (name: string) => devices.find((device) => device.name === name);
  const anomalies: AnomalyRow[] = [];
  const boiler = by("Boiler B1");
  if (boiler) {
    anomalies.push({
      device_id: boiler.device_id,
      name: boiler.name,
      kind: "abnormal",
      summary: "Exhaust temperature above the threshold",
      started_at: "2026-11-04T08:40:00Z",
      ended_at: "2026-11-04T08:55:00Z",
    });
  }
  const compressor = by("Compressor A2");
  if (compressor) {
    anomalies.push({
      device_id: compressor.device_id,
      name: compressor.name,
      kind: "offline",
      summary: "No readings",
      started_at: "2026-11-04T09:02:00Z",
      ended_at: null,
    });
  }
  return anomalies;
}

export class FakeHomeOverviewServer implements HomeOverviewApi {
  devices: DeviceHealthRow[];
  anomalies: AnomalyRow[];
  gatewayCount: number;
  updatedAt = "2026-11-04T10:24:00Z";
  since = "2026-10-31T09:40:00Z";
  /** Every call, in order. */
  readonly calls: OverviewOperation[] = [];
  private readonly failures = new Map<OverviewOperation, ApiError[]>();

  constructor(devices: DeviceHealthRow[] = demoDevices(), gatewayCount = 2) {
    this.devices = devices;
    this.anomalies = demoAnomalies(devices);
    this.gatewayCount = gatewayCount;
  }

  failNext(operation: OverviewOperation, error: ApiError = unavailable()): void {
    this.failures.set(operation, [...(this.failures.get(operation) ?? []), error]);
  }

  private async enter(operation: OverviewOperation): Promise<void> {
    this.calls.push(operation);
    await Promise.resolve();
    const failure = this.failures.get(operation)?.shift();
    if (failure !== undefined) throw failure;
  }

  async getStatusSummary(): Promise<StatusSummary> {
    await this.enter("summary");
    const of = (status: DeviceHealthRow["status"]) =>
      this.devices.filter((device) => device.status === status).map(({ device_id, name }) => ({ device_id, name }));
    return {
      updated_at: this.updatedAt,
      gateway_count: this.gatewayCount,
      normal: of("normal"),
      abnormal: of("abnormal"),
      offline: of("offline"),
    };
  }

  async getTopUsage(): Promise<TopUsage> {
    await this.enter("usage");
    const devices = this.devices
      .filter((device) => (device.usage_today_kwh ?? 0) > 0)
      .sort((a, b) => (b.usage_today_kwh ?? 0) - (a.usage_today_kwh ?? 0))
      .slice(0, 5)
      .map(({ device_id, name, usage_today_kwh }) => ({ device_id, name, kwh: usage_today_kwh ?? 0 }));
    return { since: "2026-11-04T00:00:00Z", devices };
  }

  async getNewAnomalies(): Promise<NewAnomalies> {
    await this.enter("anomalies");
    return { since: this.since, anomalies: structuredClone(this.anomalies) };
  }

  async listDeviceHealth(): Promise<DeviceHealthRow[]> {
    await this.enter("devices");
    return structuredClone(this.devices);
  }

  api(): HomeOverviewApi {
    return this;
  }
}

/** For the UI preview (Laysi/trustiot-v2#1065): "normal" is the design's demo tenant, "empty" one with no gateway. */
export const preview = {
  name: "Generic home",
  provider: (seed: "normal" | "empty") => {
    const server = seed === "empty" ? new FakeHomeOverviewServer([], 0) : new FakeHomeOverviewServer();
    return function OverviewPreviewProvider({ children }: { children: ReactNode }) {
      return createElement(HomeOverviewApiProvider, { api: server, children });
    };
  },
};
