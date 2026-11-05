// Pending Laysi/trustiot-v2#114 (binding) and Laysi/trustiot-v2#118 (health): replace with a module over the generated client when api/openapi.json has these endpoints.
import type { Gateway, GatewayApi } from "../gateways.ts";

/**
 * The gateway page's endpoints that api/openapi.json does not have yet. The shapes come from
 * docs/design/tenant-admin/design.md (3.3 and the 2026-11-01 decisions) and the two backend
 * issues; what they leave open is listed in the pull request that added this file:
 *
 * - Binding (Laysi/trustiot-v2#114): a tenant administrator sends the gateway's hardware ID and
 *   the claim code printed on its label (decision 5). The server answers the new binding as a
 *   `Gateway`. A rejected pair is one answer whether the hardware ID is not in the device pool or
 *   is bound to another tenant (#114: no enumeration), so the page cannot tell them apart.
 * - Health (Laysi/trustiot-v2#118): the devices' states per gateway and the gateway's latest
 *   operating metrics (Laysi/trustiot-v2#117), one batch for the page instead of one request per
 *   gateway (design.md section 7).
 */

/** What the gateway reported in its latest heartbeat (Laysi/trustiot-v2#117). */
export interface GatewayMetrics {
  /** When the heartbeat that carried these values arrived. */
  reported_at: string;
  cpu_percent: number;
  memory_percent: number;
  data_partition_percent: number;
  /** `null` when the sensor could not be read. */
  soc_temperature_c: number | null;
  uptime_secs: number;
  /** Restarts in the last 7 days. */
  restarts_7d: number;
  /** The power supply reported undervoltage. */
  undervoltage: boolean;
  /** Readings the gateway holds back while it cannot reach the cloud. */
  offline_buffer_rows: number;
  offline_buffer_oldest_at: string | null;
  clock_synced: boolean;
  certificate_expires_at: string | null;
}

/** How many of a gateway's devices are in each state (Laysi/trustiot-v2#118; `unknown` while the gateway is offline). */
export interface DeviceHealthCounts {
  total: number;
  normal: number;
  partial: number;
  offline: number;
  unknown: number;
}

export interface GatewayHealth {
  hardware_id: string;
  devices: DeviceHealthCounts;
  /** `null` until the gateway has sent its first heartbeat. */
  metrics: GatewayMetrics | null;
}

/** What the binding form sends. */
export interface BindGatewayRequest {
  hardwareId: string;
  /** The claim code from the label, as typed (trimmed). */
  claimCode: string;
}

/**
 * The endpoints that do not exist yet. Failures are `ApiError`s: `rate_limited` carries
 * `details.retry_after_secs`, `quota_exceeded` means the plan's gateway limit is reached, and
 * any other client error is a rejected pair.
 */
export interface PendingGatewayApi {
  /** The health of every gateway of the tenant. */
  listGatewayHealth(signal?: AbortSignal): Promise<GatewayHealth[]>;
  /** Binds the gateway to the caller's tenant (administrators only); retrying a failed call is safe. */
  bindGateway(request: BindGatewayRequest, signal?: AbortSignal): Promise<Gateway>;
}

/** Everything the gateway page reads and writes: the real endpoints and the pending ones. */
export type GatewaysApi = GatewayApi & PendingGatewayApi;
