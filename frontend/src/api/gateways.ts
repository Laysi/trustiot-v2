import type { AllTrue, Assert, Equal } from "../lib/type-binding.ts";
import { z } from "../lib/zod.ts";
import type { ApiClient } from "./client.ts";
import { pinnable, unwrap } from "./client.ts";
import type { components } from "./schema.gen.ts";

type Spec = components["schemas"];

// The gateway list of the gateway-ops service (Laysi/trustiot-v2#112) and the part of the tenant's
// entitlements the gateway page needs (identity service, Laysi/trustiot-v2#110): the plan and how
// many gateways it allows. Each schema is bound to the generated type, so a change in
// api/openapi.json fails `typecheck` (lib/type-binding.ts).
const PresenceSchema = z.enum(["online", "offline", "unknown"]);
const OfflineReasonSchema = z.enum(["shutdown", "connection_lost", "heartbeat_timeout"]);
const GatewaySchema = z.object({
  hardware_id: z.string(),
  binding_id: z.string(),
  bound_at: z.string(),
  presence: PresenceSchema,
  offline_reason: OfflineReasonSchema.nullable().optional(),
  presence_changed_at: z.string().nullable().optional(),
  last_seen_at: z.string().nullable().optional(),
  applied_config_version: z.number().int().nullable().optional(),
  desired_config_version: z.number().int().nullable().optional(),
  agent_version: z.string().nullable().optional(),
});
const GatewayListSchema = z.object({ gateways: z.array(GatewaySchema) });

const PlanSchema = z.enum(["starter", "business", "enterprise", "enterprise_on_prem"]);
const LimitSchema = z.union([z.number().int().nonnegative(), z.literal("unlimited")]);
/** Only what the page reads: the other entitlements are ignored, so a new one never breaks it. */
const GatewayAllowanceSchema = z.object({
  plan: PlanSchema,
  entitlements: z.object({ max_gateways: LimitSchema }),
});

export type GatewayBodiesMatchSpec = Assert<
  AllTrue<
    [
      Equal<z.output<typeof PresenceSchema>, Spec["Presence"]>,
      Equal<z.output<typeof OfflineReasonSchema>, Spec["OfflineReason"]>,
      Equal<z.output<typeof GatewaySchema>, Spec["Gateway"]>,
      Equal<z.output<typeof GatewayListSchema>, Spec["GatewayList"]>,
      Equal<z.output<typeof PlanSchema>, Spec["Plan"]>,
      Equal<z.output<typeof LimitSchema>, Spec["Limit"]>,
      // The part of `CurrentEntitlements` that is read is assignable from the full body.
      Spec["CurrentEntitlements"] extends z.output<typeof GatewayAllowanceSchema> ? true : false,
    ]
  >
>;

export type Gateway = z.output<typeof GatewaySchema>;
export type Presence = z.output<typeof PresenceSchema>;
export type OfflineReason = z.output<typeof OfflineReasonSchema>;
export type Plan = z.output<typeof PlanSchema>;

/** How many gateways the tenant's plan allows. */
export interface GatewayAllowance {
  plan: Plan;
  /** A number, or `unlimited`. */
  maxGateways: number | "unlimited";
}

/** The endpoints the gateway page reads. */
export interface GatewayApi {
  /** The gateways bound to the tenant, by hardware ID. */
  listGateways(signal?: AbortSignal): Promise<Gateway[]>;
  /** The tenant's plan and gateway limit (`GET /api/identity/v1/entitlements`). */
  getGatewayAllowance(signal?: AbortSignal): Promise<GatewayAllowance>;
}

/** The gateway-ops and entitlement endpoints, through the typed client (`pinnable`: a page binds its own copy). */
export function gatewayApi(client: ApiClient): GatewayApi {
  return pinnable(client, (client) => ({
    listGateways: async (signal) =>
      (await unwrap(GatewayListSchema, client.GET("/api/gateway-ops/v1/gateways", signal ? { signal } : {}))).gateways,
    getGatewayAllowance: async (signal) => {
      const body = await unwrap(
        GatewayAllowanceSchema,
        client.GET("/api/identity/v1/entitlements", signal ? { signal } : {}),
      );
      return { plan: body.plan, maxGateways: body.entitlements.max_gateways };
    },
  }));
}
