import { createElement } from "react";

import { createApiClient } from "../api/client.ts";
import { ApiError, type ErrorBody } from "../api/errors.ts";
import { type Gateway, gatewayApi, type Plan } from "../api/gateways.ts";
import { GatewaysApiProvider } from "../api/pending/GatewaysApiProvider.tsx";
import type {
  BindGatewayRequest,
  GatewayHealth,
  GatewayMetrics,
  GatewaysApi,
  PendingGatewayApi,
} from "../api/pending/gateways.ts";
import type { components } from "../api/schema.gen.ts";
import type { ProviderPreviewDeclaration } from "../ui-preview/contract.ts";
import { fakeUuid } from "./fake-devices.ts";

/**
 * The gateway page's endpoints, held in memory. The two that exist are answered at the HTTP
 * level, so the page goes through the real client and zod parsing: the tenant's gateways
 * (`GET /api/gateway-ops/v1/gateways`) and its entitlements (`GET /api/identity/v1/entitlements`,
 * of which the page reads the plan and `max_gateways`). Binding and health are pending
 * (api/pending/gateways.ts) and implemented here directly:
 *
 * - A gateway is in the device pool until it is bound: `claims` holds the claim code of each
 *   hardware ID in the pool. A wrong code, an unknown ID and an ID bound elsewhere are the same
 *   rejection (Laysi/trustiot-v2#114).
 * - Binding past the plan's limit is 403 `quota_exceeded`.
 * - In the UI preview these hardware IDs trigger a failure on purpose, so each failure state can be
 *   clicked: `tg-network` (no response) and `tg-limited` (rate limited for 30 s).
 *
 * `failNext` answers the next request to an HTTP endpoint with an error and `failNextBind` the
 * next binding; every request and binding is recorded.
 */

type Endpoint = "gateways" | "entitlements";
type Spec = components["schemas"];

function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}

export function fakeGatewayError(status: number, body: ErrorBody): ApiError {
  return new ApiError(status, body);
}

/** The claim code of the gateway the preview's seed leaves in the pool. */
export const PREVIEW_CLAIM = { hardwareId: "tg-0005", claimCode: "7KQ2-M9XA-44TR" } as const;

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

const LIMITS: Record<Plan, number | "unlimited"> = {
  starter: 1,
  business: 5,
  enterprise: 20,
  enterprise_on_prem: "unlimited",
};

/** A full entitlements body for `plan` (docs/product/plans.md), with `maxGateways` as the limit. */
function entitlementsBody(plan: Plan, maxGateways: number | "unlimited"): Spec["CurrentEntitlements"] {
  return {
    tenant_id: fakeUuid(),
    plan,
    entitlements_version: 1,
    entitlements: {
      ai_alert_rules: plan !== "starter",
      api_and_mcp: false,
      document_rag: false,
      email_notifications: plan !== "starter",
      field_context: false,
      hour_queryable_months: 12,
      max_alert_rules: plan === "starter" ? 0 : "unlimited",
      max_gateways: maxGateways,
      max_members: plan === "starter" ? 3 : 10,
      min_sampling_interval_ms: 60_000,
      minute_queryable_months: 1,
      raw_queryable_days: 7,
      realtime_charts: false,
      reports: false,
      sso: false,
      webhook_notifications: false,
    },
  };
}

export class FakeGatewayServer implements PendingGatewayApi {
  readonly gateways = new Map<string, Gateway>();
  readonly health = new Map<string, GatewayHealth>();
  /** The device pool: hardware ID to claim code, for gateways bound to nobody. */
  readonly claims = new Map<string, string>();
  plan: Plan = "business";
  maxGateways: number | "unlimited" = LIMITS.business;
  /** Every HTTP request's URL, in order. */
  readonly requests: URL[] = [];
  /** Every binding that was sent, in order. */
  readonly bindings: BindGatewayRequest[] = [];
  /** How long a binding takes, so a test or the preview can see the pending state. */
  bindDelayMs = 0;
  /** The server's clock. */
  now: () => number = () => Date.now();
  private readonly failures = new Map<Endpoint, Response[]>();
  private readonly bindFailures: ApiError[] = [];
  private healthFailure: ApiError | null = null;
  private gate: Promise<void> | null = null;

  /** The plan and its limit (`maxGateways` overrides the plan's own). */
  setPlan(plan: Plan, maxGateways: number | "unlimited" = LIMITS[plan]): void {
    this.plan = plan;
    this.maxGateways = maxGateways;
  }

  /** Puts a gateway bound `ago` ms ago in the tenant; `report` is how long ago it last reported. */
  addGateway(
    hardwareId: string,
    options: {
      presence?: Gateway["presence"];
      offlineReason?: NonNullable<Gateway["offline_reason"]>;
      since?: number;
      reportedAgo?: number;
      boundAgo?: number;
      applied?: number | null;
      desired?: number | null;
      agent?: string | null;
      devices?: Partial<GatewayHealth["devices"]>;
      metrics?: Partial<GatewayMetrics> | null;
    } = {},
  ): Gateway {
    const now = this.now();
    const presence = options.presence ?? "online";
    const reported = presence === "unknown" ? null : new Date(now - (options.reportedAgo ?? 12_000)).toISOString();
    const gateway: Gateway = {
      hardware_id: hardwareId,
      binding_id: fakeUuid(),
      bound_at: new Date(now - (options.boundAgo ?? 3 * DAY)).toISOString(),
      presence,
      offline_reason: presence === "offline" ? (options.offlineReason ?? "heartbeat_timeout") : null,
      presence_changed_at: presence === "unknown" ? null : new Date(now - (options.since ?? 2 * DAY)).toISOString(),
      last_seen_at: reported,
      applied_config_version: presence === "unknown" ? null : (options.applied ?? 14),
      desired_config_version: presence === "unknown" ? null : (options.desired ?? 14),
      agent_version: presence === "unknown" ? null : (options.agent ?? "1.4.0"),
    };
    const counts = { normal: 0, partial: 0, offline: 0, unknown: 0, ...options.devices };
    const total = options.devices?.total ?? counts.normal + counts.partial + counts.offline + counts.unknown;
    const metrics: GatewayMetrics | null =
      options.metrics === null || presence === "unknown"
        ? null
        : {
            reported_at: reported ?? new Date(now).toISOString(),
            cpu_percent: 38,
            memory_percent: 61,
            data_partition_percent: 72,
            soc_temperature_c: 71,
            uptime_secs: 3 * 86_400 + 6 * 3600,
            restarts_7d: 2,
            undervoltage: false,
            offline_buffer_rows: 0,
            offline_buffer_oldest_at: null,
            clock_synced: true,
            certificate_expires_at: "2027-04-30T00:00:00Z",
            ...options.metrics,
          };
    this.gateways.set(hardwareId, gateway);
    this.health.set(hardwareId, { hardware_id: hardwareId, devices: { ...counts, total }, metrics });
    return gateway;
  }

  /** A gateway in the device pool: bindable with `claimCode`. */
  addToPool(hardwareId: string, claimCode: string): void {
    this.claims.set(hardwareId, claimCode);
  }

  /** The next request to `endpoint` gets `response` instead of its answer. */
  failNext(endpoint: Endpoint, response: Response): void {
    this.failures.set(endpoint, [...(this.failures.get(endpoint) ?? []), response]);
  }

  /** The next binding fails with `error` (and nothing is bound). */
  failNextBind(error: ApiError): void {
    this.bindFailures.push(error);
  }

  /** Bindings wait, as if the server were slow, until the returned function is called. */
  holdBinds(): () => void {
    let release: () => void = () => {};
    this.gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    return () => {
      this.gate = null;
      release();
    };
  }

  /** Every health request fails with `error` until `null`. */
  failHealth(error: ApiError | null): void {
    this.healthFailure = error;
  }

  /** The page's API: the real client against this server, and the pending endpoints in memory. */
  api(): GatewaysApi {
    const real = gatewayApi(createApiClient({ fetch: (request) => this.fetch(request) }));
    return {
      listGateways: (signal) => real.listGateways(signal),
      getGatewayAllowance: (signal) => real.getGatewayAllowance(signal),
      listGatewayHealth: () => this.listGatewayHealth(),
      bindGateway: (request) => this.bindGateway(request),
    };
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    this.requests.push(url);
    const endpoint: Endpoint | null =
      url.pathname === "/api/gateway-ops/v1/gateways"
        ? "gateways"
        : url.pathname === "/api/identity/v1/entitlements"
          ? "entitlements"
          : null;
    if (endpoint === null) return json(404, { code: "not_found", message_key: "error.not_found" });
    if (request.method !== "GET") return json(405, { code: "invalid_request", message_key: "error.invalid_request" });
    const failure = this.failures.get(endpoint)?.shift();
    if (failure) return failure;
    if (endpoint === "entitlements") return json(200, entitlementsBody(this.plan, this.maxGateways));
    const gateways = [...this.gateways.values()].sort((a, b) => (a.hardware_id < b.hardware_id ? -1 : 1));
    return json(200, { gateways });
  }

  async listGatewayHealth(): Promise<GatewayHealth[]> {
    if (this.healthFailure) throw this.healthFailure;
    return [...this.health.values()];
  }

  async bindGateway(request: BindGatewayRequest): Promise<Gateway> {
    this.bindings.push(request);
    if (this.gate) await this.gate;
    if (this.bindDelayMs > 0) await new Promise((resolve) => setTimeout(resolve, this.bindDelayMs));
    const failure = this.bindFailures.shift();
    if (failure) throw failure;
    if (request.hardwareId === "tg-network") throw new ApiError(0, null);
    if (request.hardwareId === "tg-limited") {
      throw new ApiError(429, {
        code: "rate_limited",
        message_key: "error.rate_limited",
        details: { retry_after_secs: 30 },
      });
    }
    // The same answer for an ID that is not in the pool, one bound to another tenant (not in the
    // pool either) and a wrong code.
    if (this.claims.get(request.hardwareId) !== request.claimCode) {
      throw new ApiError(404, { code: "not_found", message_key: "gateway.claim_rejected" });
    }
    if (this.maxGateways !== "unlimited" && this.gateways.size >= this.maxGateways) {
      throw new ApiError(403, { code: "quota_exceeded", message_key: "gateway.quota_exceeded" });
    }
    this.claims.delete(request.hardwareId);
    return this.addGateway(request.hardwareId, { presence: "unknown", boundAgo: 0 });
  }
}

/** The preview's demo data: the design's four gateways, and one more in the device pool. */
export function demoGatewayServer(seed: "normal" | "empty"): FakeGatewayServer {
  const server = new FakeGatewayServer();
  server.addToPool(PREVIEW_CLAIM.hardwareId, PREVIEW_CLAIM.claimCode);
  // Long enough to see the button say "Binding…".
  server.bindDelayMs = 400;
  if (seed === "empty") return server;
  server.addGateway("tg-0001", {
    since: 2 * DAY,
    devices: { normal: 1, partial: 1, offline: 1 },
  });
  server.addGateway("tg-0002", {
    presence: "offline",
    offlineReason: "heartbeat_timeout",
    since: 4 * HOUR,
    reportedAgo: 4 * HOUR,
    applied: 9,
    desired: 9,
    devices: { unknown: 1 },
    metrics: {
      undervoltage: true,
      offline_buffer_rows: 1240,
      offline_buffer_oldest_at: new Date(Date.now() - 4 * HOUR).toISOString(),
    },
  });
  server.addGateway("tg-0003", {
    since: 4 * HOUR,
    reportedAgo: 40_000,
    applied: 3,
    desired: 4,
    agent: "1.3.2",
    devices: { normal: 6 },
    metrics: { clock_synced: false },
  });
  server.addGateway("tg-0004", { presence: "unknown", boundAgo: 14 * MINUTE });
  return server;
}

/**
 * The UI preview's gateways (a provider: the binding and health endpoints are pending, so the
 * fake is the whole API). `normal` is the design's four gateways on a Business plan (5 allowed), so
 * binding one more (hardware ID `${PREVIEW_CLAIM.hardwareId}`, claim code `${PREVIEW_CLAIM.claimCode}`)
 * fills the plan; `empty` has none.
 */
export const preview: ProviderPreviewDeclaration = {
  name: "gateways",
  provider: (seed) => {
    const api = demoGatewayServer(seed).api();
    return ({ children }) => createElement(GatewaysApiProvider, { api, children });
  },
};
