// Pending Laysi/trustiot-v2#131: replace with a module over the generated client when api/openapi.json has these endpoints.
//
// The platform back office's API as its screens use it (docs/design/platform-admin/design.md,
// Laysi/trustiot-v2#132). The spec already has some of it, the rest is the backend issues' work;
// each method says which, so the switch to the generated client is one method at a time:
//
// - In api/openapi.json today: `getPlatformTenantState`, `setPlatformTenantState`,
//   `createTenant`, `getPlatformTenantEntitlements`, `setPlatformTenantEntitlements`,
//   `setPlatformTenantMemberRole`, `getPlatformEventLog`, `getPlatformTenantEventLog`,
//   `listPoolGateways`, `bindGateway`, `revokeGateway`.
// - Not yet (the backend issue in brackets): the tenant list and the tenant's usage figures
//   (#131), the first admin's invitation (#105), the member list (#131), the quota adjustment
//   (#110), the health overview (#118, #131), unbinding, sending in for repair and decommissioning
//   (#115), the user ID to email lookup that only platform administrators may call (decision Q14).
//
// The shapes are the screens', not the wire's: the page never sees a snake_case field, so the
// module that replaces this one converts at its edge (as `src/api/devices.ts` does).

export type Plan = "starter" | "business" | "enterprise" | "enterprise_on_prem";
export type TenantState = "active" | "suspended" | "deleted";
/** What the back office shows of the state: a suspension certs has not confirmed is `processing`. */
export type TenantStatus = "active" | "processing" | "suspended" | "deleted";
export type SuspensionReason = "non_payment" | "security_or_abuse" | "contract_terminated";
export type MemberRole = "viewer" | "editor" | "admin";
/** A number of things a tenant may have, or no limit. */
export type Limit = number | "unlimited";

/** One quantity of a tenant against its limit. */
export interface Usage {
  used: number;
  limit: Limit;
}

/** What a plan gives, for the "plan default" hints next to a limit that can be overridden. */
export interface PlanDefaults {
  maxGateways: Limit;
  maxMembers: Limit;
  maxAlertRules: Limit;
  aiMonthlyUsd: number;
}

export interface TenantSummary {
  id: string;
  name: string;
  plan: Plan;
  state: TenantState;
  status: TenantStatus;
  /** Why it is suspended; `null` while it is active. */
  reason: SuspensionReason | null;
  /** Goes up by one on every change of the state or its reason: the order of changes. */
  stateVersion: number;
  /** When the tenant entered this state (the database's clock). */
  stateChangedAt: string;
  /** The administrator of the latest change; `null` for a tenant never changed since it was created. */
  changedByUserId: string | null;
  createdAt: string;
  gateways: Usage;
  members: Usage;
  alertRules: Usage;
  ai: {
    /** This month's spend, US dollars (UTC month). */
    usedUsd: number;
    /** The plan's monthly allowance, with the temporary raise. */
    limitUsd: number;
    planLimitUsd: number;
    /** The raise, `0` for none, valid to `extraUntil` (the month's last day). */
    extraUsd: number;
    extraUntil: string | null;
  };
  planDefaults: PlanDefaults;
}

export interface TenantMember {
  userId: string;
  email: string;
  role: MemberRole;
}

/** The values an Enterprise contract sets apart from its plan; absent means the plan's value. */
export interface EnterpriseOverrides {
  maxGateways?: Limit;
  maxMembers?: Limit;
  maxAlertRules?: Limit;
  aiMonthlyUsd?: number;
}

export interface TenantStateSnapshot {
  tenantId: string;
  state: TenantState;
  status: TenantStatus;
  reason: SuspensionReason | null;
  stateVersion: number;
  stateChangedAt: string;
}

/** Who made a logged change. */
export type LoggedActor = { kind: "user"; userId: string } | { kind: "service"; service: string };

/** One event of the operation log; `payload` is the producer's, as it wrote it. */
export interface LoggedEvent {
  eventId: string;
  type: string;
  version: number;
  producer: string;
  /** `null` for a platform-wide event. */
  tenantId: string | null;
  aggregateType: string;
  aggregateId: string;
  aggregateSeq: number;
  occurredAt: string;
  recordedAt: string;
  actor: LoggedActor | null;
  payload: unknown;
}

export type EventScope = { kind: "platform" } | { kind: "tenant"; tenantId: string };

export interface EventQuery {
  scope: EventScope;
  /** Inclusive. */
  from?: string;
  /** Exclusive. */
  to?: string;
  cursor?: string;
}

export interface EventPage {
  events: LoggedEvent[];
  nextCursor: string | null;
}

export type Health = "online" | "degraded" | "offline" | "unknown";
/** Where a gateway is: waiting in the device pool, bound to a tenant, or away for repair. */
export type Placement = "pool" | "bound" | "repair";

export interface GatewayRow {
  hardwareId: string;
  placement: Placement;
  /** The tenant of a bound gateway. */
  tenant: { id: string; name: string; status: TenantStatus } | null;
  /** `null` for a gateway that is not bound: it reports nothing. */
  health: Health | null;
  /** Rows of data waiting in the gateway's buffer, for a degraded one. */
  backlog: number | null;
  lastReportAt: string | null;
  registeredAt: string;
  workstation: string;
  boundAt: string | null;
}

export interface GatewaySummary {
  total: number;
  bound: number;
  pool: number;
  online: number;
  degraded: number;
  offline: number;
}

export interface GatewayQuery {
  /** Only this tenant's gateways. */
  tenantId?: string;
  cursor?: string;
}

export interface GatewayPage {
  gateways: GatewayRow[];
  nextCursor: string | null;
  summary: GatewaySummary;
  /** When the health was read. */
  updatedAt: string;
  /** `false` when the health data could not be read: the rows still show where each gateway is. */
  healthAvailable: boolean;
}

export type RevocationReason =
  | "unspecified"
  | "key_compromise"
  | "affiliation_changed"
  | "superseded"
  | "cessation_of_operation"
  | "privilege_withdrawn";

export interface ProvisioningEntry {
  eventId: string;
  occurredAt: string;
  hardwareId: string;
  workstation: string;
  actorUserId: string | null;
  placement: Placement | "decommissioned";
  /** The tenant the gateway is bound to now. */
  tenantName: string | null;
}

export interface ProvisioningQuery {
  workstation?: string;
  from?: string;
  to?: string;
  cursor?: string;
}

export interface ProvisioningPage {
  entries: ProvisioningEntry[];
  nextCursor: string | null;
  /** Every workstation that registered a gateway, for the filter. */
  workstations: string[];
}

/** The quota change the dialog sends; a value left out stays as it is. */
export interface QuotaChange {
  /** `null` goes back to the plan's value. */
  maxGateways?: Limit | null;
  /** The raise of this month's AI allowance, in US dollars; `0` removes it. */
  aiExtraUsd?: number;
  /** Why, written to the operation log (decision Q5): required. */
  reason: string;
}

export interface PlatformApi {
  /** Pending (#131): every tenant, suspended and deleted ones included, with plan, state and usage. */
  listTenants(signal?: AbortSignal): Promise<TenantSummary[]>;
  /** Pending (#131): one tenant with the same figures as the list. 404 `not_found` for no such tenant. */
  getTenant(tenantId: string, signal?: AbortSignal): Promise<TenantSummary>;
  /** Spec: `getPlatformTenantState`. */
  getTenantState(tenantId: string, signal?: AbortSignal): Promise<TenantStateSnapshot>;
  /**
   * Spec: `setPlatformTenantState`. Suspends with a reason, changes the reason, or resumes
   * (`reason` null). `expectedStateVersion` is decision Q3 (pending #131): the server answers 409
   * when the version differs; until then the page reads the state again before sending.
   */
  setTenantState(
    tenantId: string,
    change: { state: "active" | "suspended"; reason: SuspensionReason | null; expectedStateVersion: number },
  ): Promise<TenantStateSnapshot>;

  /** Spec: `createTenant`. Not idempotent: a second call makes a second tenant of the same name. */
  createTenant(input: { name: string }): Promise<{ id: string; name: string }>;
  /** Spec: `setPlatformTenantEntitlements`; the module reads `entitlements_version` first and sends it as `expected_version`. */
  setTenantPlan(tenantId: string, input: { plan: Plan; overrides: EnterpriseOverrides }): Promise<void>;
  /** Pending (#105): invites the first admin by email; an address with no account gets one by the invitation. */
  inviteFirstAdmin(tenantId: string, email: string): Promise<void>;
  /** Pending (#110): changes the gateway limit and this month's AI raise, and records why. */
  adjustQuota(tenantId: string, change: QuotaChange): Promise<TenantSummary>;

  /** Pending (#131): the tenant's members. */
  listMembers(tenantId: string, signal?: AbortSignal): Promise<TenantMember[]>;
  /** Spec: `setPlatformTenantMemberRole`. Only a user that has an account (422 `member.email_unknown`). */
  setMemberRole(
    tenantId: string,
    input: { email: string; role: MemberRole },
  ): Promise<{ previousRole: MemberRole | null }>;

  /** Spec: `getPlatformEventLog` and `getPlatformTenantEventLog`. */
  listEvents(query: EventQuery, signal?: AbortSignal): Promise<EventPage>;
  /** Pending (decision Q14): the emails of these users, for the ones that have one; platform administrators only. */
  lookupUserEmails(userIds: readonly string[], signal?: AbortSignal): Promise<Record<string, string>>;

  /** Spec: `listPoolGateways` for the pool; pending (#118, #131) for the health and the summary. */
  listGateways(query: GatewayQuery, signal?: AbortSignal): Promise<GatewayPage>;
  /** Spec: `bindGateway`. 409 when another tenant holds it, 422 `gateway_binding.tenant_unknown`. */
  bindGateway(hardwareId: string, tenantId: string): Promise<void>;
  /** Pending (#115): revokes the certificates, clears the configuration, returns the gateway to the pool. */
  unbindGateway(hardwareId: string): Promise<void>;
  /** Pending (#115), decision Q6: unbinds, revokes and marks the gateway as sent in for repair. */
  sendForRepair(hardwareId: string, input: { note: string }): Promise<void>;
  /** Spec: `revokeGateway`. */
  revokeGatewayCertificates(hardwareId: string, input: { reason: RevocationReason }): Promise<void>;
  /** Pending (#115): irreversible; the gateway leaves the pool for good. */
  decommissionGateway(hardwareId: string): Promise<void>;

  /** Pending (#131): the factory registrations, newest first. */
  listProvisioning(query: ProvisioningQuery, signal?: AbortSignal): Promise<ProvisioningPage>;
}
