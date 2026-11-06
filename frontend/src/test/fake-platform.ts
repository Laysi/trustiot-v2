import type { ComponentType, ReactNode } from "react";
import { createElement, useEffect, useMemo } from "react";

import { ApiError, type ErrorBody } from "../api/errors.ts";
import { PlatformApiProvider } from "../api/pending/PlatformApiProvider.tsx";
import type {
  EnterpriseOverrides,
  EventPage,
  EventQuery,
  GatewayPage,
  GatewayQuery,
  GatewayRow,
  Health,
  Limit,
  LoggedActor,
  LoggedEvent,
  MemberRole,
  Placement,
  Plan,
  PlanDefaults,
  PlatformApi,
  ProvisioningEntry,
  ProvisioningPage,
  ProvisioningQuery,
  QuotaChange,
  RevocationReason,
  SuspensionReason,
  TenantMember,
  TenantState,
  TenantStateSnapshot,
  TenantStatus,
  TenantSummary,
} from "../api/pending/platform.ts";
import { fakeUuid } from "./fake-devices.ts";

/**
 * The platform back office's API in memory, with the rules the backend issues and the identity,
 * assets and certs READMEs set (Laysi/trustiot-v2#132); the interface is pending
 * (`src/api/pending/platform.ts`). It keeps the state machine of a tenant (a suspension is
 * `processing` until `confirmApplied`, as certs' confirmation does it), writes an event to the
 * operation log for every change, and refuses what the services refuse: a final state (409
 * `tenant.state_final`), a reason that does not fit the state (422), an unknown member email
 * (422 `member.email_unknown`), a gateway another tenant holds (409), an empty reason for a quota
 * change. `failNext` makes the next call to a method fail; `hold` keeps it waiting until released.
 * All people and companies are made up.
 */

export const ADMIN_USER_ID = "00000000-0000-4000-8000-0000000000a1";
export const ADMIN_EMAIL = "platform-admin@example.test";

const PAGE_SIZE = 50;
const MINUTE = 60_000;

const STARTER: PlanDefaults = { maxGateways: 1, maxMembers: 3, maxAlertRules: 0, aiMonthlyUsd: 10 };
const BUSINESS: PlanDefaults = { maxGateways: 5, maxMembers: 10, maxAlertRules: 20, aiMonthlyUsd: 50 };
const ENTERPRISE: PlanDefaults = { maxGateways: 20, maxMembers: 100, maxAlertRules: "unlimited", aiMonthlyUsd: 300 };

export function planDefaults(plan: Plan): PlanDefaults {
  if (plan === "starter") return STARTER;
  if (plan === "business") return BUSINESS;
  return ENTERPRISE;
}

function apiError(status: number, body: ErrorBody): ApiError {
  return new ApiError(status, body);
}

function fieldError(field: string, key: string, details?: Record<string, string | number>): ApiError {
  return apiError(422, {
    code: "validation_failed",
    message_key: "error.validation_failed",
    field_errors: [{ field, message_key: key, ...(details ? { details } : {}) }],
  });
}

export const notFound = () => apiError(404, { code: "not_found", message_key: "error.not_found" });
export const unavailable = () => apiError(503, { code: "unavailable", message_key: "error.unavailable" });
const conflict = (key: string) => apiError(409, { code: "conflict", message_key: key });

interface TenantRecord {
  id: string;
  name: string;
  plan: Plan;
  overrides: EnterpriseOverrides;
  state: TenantState;
  reason: SuspensionReason | null;
  stateVersion: number;
  stateChangedAt: Date;
  changedByUserId: string | null;
  /** The newest state version certs confirmed. */
  appliedVersion: number;
  createdAt: Date;
  members: TenantMember[];
  gateways: number;
  alertRules: number;
  aiUsedUsd: number;
  aiExtraUsd: number;
}

interface GatewayRecord {
  hardwareId: string;
  placement: Placement;
  tenantId: string | null;
  health: Health;
  backlog: number | null;
  lastReportAt: Date | null;
  registeredAt: Date;
  workstation: string;
  boundAt: Date | null;
  revoked: boolean;
}

type Method = keyof PlatformApi;

/** What the seed puts in: the normal state a screen is designed for, or nothing at all. */
export type FakePlatformSeed = "normal" | "empty";

export class FakePlatform implements PlatformApi {
  readonly tenants = new Map<string, TenantRecord>();
  readonly gateways = new Map<string, GatewayRecord>();
  readonly events: LoggedEvent[] = [];
  readonly users = new Map<string, string>([[ADMIN_USER_ID, ADMIN_EMAIL]]);
  /** Every call, as `method` and its first argument. */
  readonly calls: string[] = [];
  /** The server's clock. */
  now: () => Date = () => new Date();
  /** Gateways' health could not be read: the list answers `healthAvailable: false`. */
  healthUnavailable = false;
  /** Rows to a page of the event log, the gateways and the provisioning list. */
  pageSize = PAGE_SIZE;
  /** A suspension is confirmed by certs this long after it is written; `null` waits for `confirmApplied`. */
  autoConfirmMs: number | null = null;

  private readonly failures = new Map<Method, unknown[]>();
  private readonly holds = new Map<Method, Promise<void>>();
  private readonly releases = new Map<Method, () => void>();
  private readonly timers = new Set<ReturnType<typeof setTimeout>>();
  private decommissioned = new Set<string>();
  private registrations: ProvisioningEntry[] = [];

  constructor(seed: FakePlatformSeed = "empty", options: { now?: () => Date } = {}) {
    if (options.now) this.now = options.now;
    if (seed === "normal") this.seedNormal();
  }

  /** Stops the pending auto-confirmations (a preview unmounting). */
  dispose(): void {
    for (const timer of this.timers) clearTimeout(timer);
    this.timers.clear();
  }

  /** The next call to `method` fails with `error` (a 503 `unavailable` unless given). */
  failNext(method: Method, error: unknown = unavailable()): void {
    this.failures.set(method, [...(this.failures.get(method) ?? []), error]);
  }

  /** Every call to `method` waits until `release(method)`. */
  hold(method: Method): void {
    this.holds.set(
      method,
      new Promise<void>((resolve) => {
        this.releases.set(method, resolve);
      }),
    );
  }

  release(method: Method): void {
    this.releases.get(method)?.();
    this.holds.delete(method);
    this.releases.delete(method);
  }

  /** What certs does after it revoked the certificates and kicked the gateways of a suspended tenant. */
  confirmApplied(tenantId: string): void {
    const tenant = this.tenants.get(tenantId);
    if (!tenant) return;
    tenant.appliedVersion = tenant.stateVersion;
    this.record({
      type: "certs.tenant_state_applied",
      producer: "certs",
      tenantId,
      aggregateType: "tenant_state",
      aggregateId: tenantId,
      actor: { kind: "service", service: "certs" },
      payload: { state_version: tenant.stateVersion },
    });
  }

  addTenant(name: string, init: Partial<Omit<TenantRecord, "id" | "name">> & { plan?: Plan } = {}): TenantRecord {
    const plan = init.plan ?? "starter";
    const now = this.now();
    const tenant: TenantRecord = {
      id: fakeUuid(),
      name,
      plan,
      overrides: {},
      state: "active",
      reason: null,
      stateVersion: 1,
      stateChangedAt: now,
      changedByUserId: null,
      appliedVersion: 0,
      createdAt: now,
      members: [],
      gateways: 0,
      alertRules: 0,
      aiUsedUsd: 0,
      aiExtraUsd: 0,
      ...init,
    };
    this.tenants.set(tenant.id, tenant);
    return tenant;
  }

  addMember(tenant: TenantRecord, email: string, role: MemberRole): TenantMember {
    let userId = [...this.users].find(([, known]) => known === email)?.[0];
    if (userId === undefined) {
      userId = fakeUuid();
      this.users.set(userId, email);
    }
    const member: TenantMember = { userId, email, role };
    tenant.members.push(member);
    return member;
  }

  addGateway(init: Partial<GatewayRecord> & { hardwareId: string }): GatewayRecord {
    const gateway: GatewayRecord = {
      placement: "pool",
      tenantId: null,
      health: "unknown",
      backlog: null,
      lastReportAt: null,
      registeredAt: this.now(),
      workstation: "WS-01",
      boundAt: null,
      revoked: false,
      ...init,
    };
    this.gateways.set(gateway.hardwareId, gateway);
    this.registrations.push({
      eventId: fakeUuid(),
      occurredAt: gateway.registeredAt.toISOString(),
      hardwareId: gateway.hardwareId,
      workstation: gateway.workstation,
      actorUserId: ADMIN_USER_ID,
      placement: gateway.placement,
      tenantName: null,
    });
    this.record({
      type: "certs.gateway_registered",
      producer: "certs",
      tenantId: null,
      aggregateType: "gateway_registration",
      aggregateId: gateway.hardwareId,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { hardware_id: gateway.hardwareId, workstation: gateway.workstation },
      occurredAt: gateway.registeredAt,
    });
    return gateway;
  }

  record(event: {
    type: string;
    producer: string;
    tenantId: string | null;
    aggregateType: string;
    aggregateId: string;
    actor: LoggedActor | null;
    payload: unknown;
    occurredAt?: Date;
    version?: number;
  }): LoggedEvent {
    const occurred = event.occurredAt ?? this.now();
    const seq = this.events.filter((e) => e.aggregateId === event.aggregateId).length + 1;
    const logged: LoggedEvent = {
      eventId: fakeUuid(),
      type: event.type,
      version: event.version ?? 1,
      producer: event.producer,
      tenantId: event.tenantId,
      aggregateType: event.aggregateType,
      aggregateId: event.aggregateId,
      aggregateSeq: seq,
      occurredAt: occurred.toISOString(),
      recordedAt: new Date(occurred.getTime() + 600).toISOString(),
      actor: event.actor,
      payload: event.payload,
    };
    this.events.push(logged);
    return logged;
  }

  private async enter(method: Method, argument?: string): Promise<void> {
    this.calls.push(argument === undefined ? method : `${method} ${argument}`);
    await this.holds.get(method);
    const queue = this.failures.get(method);
    const error = queue?.shift();
    if (queue?.length === 0) this.failures.delete(method);
    if (error !== undefined) throw error;
  }

  private tenant(id: string): TenantRecord {
    const tenant = this.tenants.get(id);
    if (!tenant) throw notFound();
    return tenant;
  }

  private status(tenant: TenantRecord): TenantStatus {
    if (tenant.state === "active") return "active";
    return tenant.appliedVersion >= tenant.stateVersion ? tenant.state : "processing";
  }

  private limit(tenant: TenantRecord, key: "maxGateways" | "maxMembers" | "maxAlertRules"): Limit {
    return tenant.overrides[key] ?? planDefaults(tenant.plan)[key];
  }

  private summary(tenant: TenantRecord): TenantSummary {
    const defaults = planDefaults(tenant.plan);
    const planLimit = tenant.overrides.aiMonthlyUsd ?? defaults.aiMonthlyUsd;
    const now = this.now();
    const monthEnd = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + 1, 0));
    return {
      id: tenant.id,
      name: tenant.name,
      plan: tenant.plan,
      state: tenant.state,
      status: this.status(tenant),
      reason: tenant.reason,
      stateVersion: tenant.stateVersion,
      stateChangedAt: tenant.stateChangedAt.toISOString(),
      changedByUserId: tenant.changedByUserId,
      createdAt: tenant.createdAt.toISOString(),
      gateways: { used: tenant.gateways, limit: this.limit(tenant, "maxGateways") },
      members: { used: tenant.members.length, limit: this.limit(tenant, "maxMembers") },
      alertRules: { used: tenant.alertRules, limit: this.limit(tenant, "maxAlertRules") },
      ai: {
        usedUsd: tenant.aiUsedUsd,
        limitUsd: planLimit + tenant.aiExtraUsd,
        planLimitUsd: planLimit,
        extraUsd: tenant.aiExtraUsd,
        extraUntil: tenant.aiExtraUsd > 0 ? monthEnd.toISOString().slice(0, 10) : null,
      },
      planDefaults: { ...defaults, aiMonthlyUsd: planLimit, maxGateways: defaults.maxGateways },
    };
  }

  private snapshot(tenant: TenantRecord): TenantStateSnapshot {
    return {
      tenantId: tenant.id,
      state: tenant.state,
      status: this.status(tenant),
      reason: tenant.reason,
      stateVersion: tenant.stateVersion,
      stateChangedAt: tenant.stateChangedAt.toISOString(),
    };
  }

  async listTenants(): Promise<TenantSummary[]> {
    await this.enter("listTenants");
    return [...this.tenants.values()].map((tenant) => this.summary(tenant));
  }

  async getTenant(tenantId: string): Promise<TenantSummary> {
    await this.enter("getTenant", tenantId);
    return this.summary(this.tenant(tenantId));
  }

  async getTenantState(tenantId: string): Promise<TenantStateSnapshot> {
    await this.enter("getTenantState", tenantId);
    return this.snapshot(this.tenant(tenantId));
  }

  async setTenantState(
    tenantId: string,
    change: { state: "active" | "suspended"; reason: SuspensionReason | null; expectedStateVersion: number },
  ): Promise<TenantStateSnapshot> {
    await this.enter("setTenantState", tenantId);
    const tenant = this.tenant(tenantId);
    if (tenant.state === "deleted" || tenant.reason === "contract_terminated") {
      throw conflict("tenant.state_final");
    }
    if (change.state === "active" && change.reason !== null)
      throw fieldError("/reason", "tenant.suspension_reason_invalid");
    if (change.state === "suspended" && change.reason === null)
      throw fieldError("/reason", "tenant.suspension_reason_invalid");
    if (change.expectedStateVersion !== tenant.stateVersion) throw conflict("error.conflict");
    if (tenant.state === change.state && tenant.reason === change.reason) return this.snapshot(tenant);
    tenant.state = change.state;
    tenant.reason = change.reason;
    tenant.stateVersion += 1;
    tenant.stateChangedAt = this.now();
    tenant.changedByUserId = ADMIN_USER_ID;
    if (change.state === "active") tenant.appliedVersion = tenant.stateVersion;
    this.record({
      type: "identity.tenant_state_changed",
      producer: "identity",
      tenantId,
      aggregateType: "tenant",
      aggregateId: tenantId,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { state: tenant.state, reason: tenant.reason, state_version: tenant.stateVersion },
    });
    if (change.state === "suspended" && this.autoConfirmMs !== null) {
      const version = tenant.stateVersion;
      const timer = setTimeout(() => {
        this.timers.delete(timer);
        if (tenant.stateVersion === version) this.confirmApplied(tenantId);
      }, this.autoConfirmMs);
      this.timers.add(timer);
    }
    return this.snapshot(tenant);
  }

  async createTenant(input: { name: string }): Promise<{ id: string; name: string }> {
    await this.enter("createTenant");
    const name = input.name.trim();
    if (name === "" || name.length > 100) throw fieldError("/name", "tenant.name_invalid", { max: 100 });
    const tenant = this.addTenant(name);
    this.record({
      type: "identity.tenant_created",
      producer: "identity",
      tenantId: tenant.id,
      aggregateType: "tenant",
      aggregateId: tenant.id,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { name },
    });
    return { id: tenant.id, name };
  }

  async setTenantPlan(tenantId: string, input: { plan: Plan; overrides: EnterpriseOverrides }): Promise<void> {
    await this.enter("setTenantPlan", tenantId);
    const tenant = this.tenant(tenantId);
    const overrides = Object.fromEntries(Object.entries(input.overrides).filter(([, value]) => value !== undefined));
    if (Object.keys(overrides).length > 0 && input.plan !== "enterprise" && input.plan !== "enterprise_on_prem") {
      throw fieldError("/overrides", "entitlements.overrides_not_allowed");
    }
    tenant.plan = input.plan;
    tenant.overrides = overrides;
    this.record({
      type: "identity.tenant_entitlements_changed",
      producer: "identity",
      tenantId,
      aggregateType: "tenant_entitlements",
      aggregateId: tenantId,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { plan: input.plan, entitlements_version: 2 },
    });
  }

  async inviteFirstAdmin(tenantId: string, email: string): Promise<void> {
    await this.enter("inviteFirstAdmin", tenantId);
    const tenant = this.tenant(tenantId);
    if (!/^[^@\s]+@[^@\s]+$/.test(email)) throw fieldError("/email", "member.email_invalid");
    const member = this.addMember(tenant, email, "admin");
    this.record({
      type: "identity.member_role_changed",
      producer: "identity",
      tenantId,
      aggregateType: "membership",
      aggregateId: `${tenantId}:${member.userId}`,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { user_id: member.userId, previous_role: null, role: "admin" },
    });
  }

  async adjustQuota(tenantId: string, change: QuotaChange): Promise<TenantSummary> {
    await this.enter("adjustQuota", tenantId);
    const tenant = this.tenant(tenantId);
    if (tenant.state === "deleted" || tenant.reason === "contract_terminated") throw conflict("tenant.state_final");
    if (change.reason.trim() === "") throw fieldError("/reason", "error.validation_failed");
    const payload: Record<string, unknown> = { reason: change.reason.trim() };
    if (change.maxGateways !== undefined) {
      const before = this.limit(tenant, "maxGateways");
      if (change.maxGateways === null) delete tenant.overrides.maxGateways;
      else tenant.overrides.maxGateways = change.maxGateways;
      payload.max_gateways = { from: before, to: this.limit(tenant, "maxGateways") };
    }
    if (change.aiExtraUsd !== undefined) {
      payload.ai_extra_usd = { from: tenant.aiExtraUsd, to: change.aiExtraUsd };
      tenant.aiExtraUsd = change.aiExtraUsd;
    }
    this.record({
      type: "identity.quota_changed",
      producer: "identity",
      tenantId,
      aggregateType: "tenant_quota",
      aggregateId: tenantId,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload,
    });
    return this.summary(tenant);
  }

  async listMembers(tenantId: string): Promise<TenantMember[]> {
    await this.enter("listMembers", tenantId);
    return this.tenant(tenantId).members.map((member) => ({ ...member }));
  }

  async setMemberRole(tenantId: string, input: { email: string; role: MemberRole }) {
    await this.enter("setMemberRole", tenantId);
    const tenant = this.tenant(tenantId);
    const email = input.email.trim();
    if (!/^[^@\s]+@[^@\s]+$/.test(email)) throw fieldError("/email", "member.email_invalid");
    const userId = [...this.users].find(([, known]) => known === email)?.[0];
    if (userId === undefined) throw fieldError("/email", "member.email_unknown");
    const existing = tenant.members.find((member) => member.userId === userId);
    const previousRole = existing?.role ?? null;
    if (existing) existing.role = input.role;
    else tenant.members.push({ userId, email, role: input.role });
    if (previousRole !== input.role) {
      this.record({
        type: "identity.member_role_changed",
        producer: "identity",
        tenantId,
        aggregateType: "membership",
        aggregateId: `${tenantId}:${userId}`,
        actor: { kind: "user", userId: ADMIN_USER_ID },
        payload: { user_id: userId, previous_role: previousRole, role: input.role },
      });
    }
    return { previousRole };
  }

  async listEvents(query: EventQuery): Promise<EventPage> {
    await this.enter("listEvents", query.scope.kind === "tenant" ? query.scope.tenantId : "platform");
    if (query.scope.kind === "tenant") this.tenant(query.scope.tenantId);
    const tenantId = query.scope.kind === "tenant" ? query.scope.tenantId : null;
    const from = query.from ? Date.parse(query.from) : Number.NEGATIVE_INFINITY;
    const to = query.to ? Date.parse(query.to) : Number.POSITIVE_INFINITY;
    const matching = this.events
      .filter((event) => event.tenantId === tenantId)
      .filter((event) => Date.parse(event.occurredAt) >= from && Date.parse(event.occurredAt) < to)
      .sort((a, b) => Date.parse(b.occurredAt) - Date.parse(a.occurredAt) || b.aggregateSeq - a.aggregateSeq);
    const start = query.cursor ? Number(query.cursor) : 0;
    const events = matching.slice(start, start + this.pageSize);
    return { events, nextCursor: start + this.pageSize < matching.length ? String(start + this.pageSize) : null };
  }

  async lookupUserEmails(userIds: readonly string[]): Promise<Record<string, string>> {
    await this.enter("lookupUserEmails");
    const found: Record<string, string> = {};
    for (const id of userIds) {
      const email = this.users.get(id);
      if (email !== undefined) found[id] = email;
    }
    return found;
  }

  private row(gateway: GatewayRecord): GatewayRow {
    const tenant = gateway.tenantId ? this.tenants.get(gateway.tenantId) : undefined;
    const bound = gateway.placement === "bound";
    return {
      hardwareId: gateway.hardwareId,
      placement: gateway.placement,
      tenant: tenant ? { id: tenant.id, name: tenant.name, status: this.status(tenant) } : null,
      health: bound ? (this.healthUnavailable ? "unknown" : gateway.health) : null,
      backlog: gateway.backlog,
      lastReportAt: gateway.lastReportAt?.toISOString() ?? null,
      registeredAt: gateway.registeredAt.toISOString(),
      workstation: gateway.workstation,
      boundAt: gateway.boundAt?.toISOString() ?? null,
    };
  }

  async listGateways(query: GatewayQuery): Promise<GatewayPage> {
    await this.enter("listGateways", query.tenantId);
    const all = [...this.gateways.values()].sort((a, b) => (a.hardwareId < b.hardwareId ? -1 : 1));
    const bound = all.filter((gateway) => gateway.placement === "bound");
    const shown = (query.tenantId ? all.filter((gateway) => gateway.tenantId === query.tenantId) : all).map((gateway) =>
      this.row(gateway),
    );
    const start = query.cursor ? Number(query.cursor) : 0;
    const count = (health: Health) => bound.filter((gateway) => gateway.health === health).length;
    return {
      gateways: shown.slice(start, start + this.pageSize),
      nextCursor: start + this.pageSize < shown.length ? String(start + this.pageSize) : null,
      summary: {
        total: all.length,
        bound: bound.length,
        pool: all.filter((gateway) => gateway.placement === "pool").length,
        online: count("online"),
        degraded: count("degraded"),
        offline: count("offline"),
      },
      updatedAt: this.now().toISOString(),
      healthAvailable: !this.healthUnavailable,
    };
  }

  private gateway(hardwareId: string): GatewayRecord {
    const gateway = this.gateways.get(hardwareId);
    if (!gateway) throw notFound();
    return gateway;
  }

  async bindGateway(hardwareId: string, tenantId: string): Promise<void> {
    await this.enter("bindGateway", hardwareId);
    const gateway = this.gateway(hardwareId);
    const tenant = this.tenants.get(tenantId);
    if (tenant?.state !== "active") throw fieldError("/tenant_id", "gateway_binding.tenant_unknown");
    if (gateway.placement === "bound" && gateway.tenantId === tenantId) return;
    if (gateway.placement !== "pool") throw conflict("gateway_binding.bound_to_another_tenant");
    const limit = this.limit(tenant, "maxGateways");
    if (limit !== "unlimited" && tenant.gateways >= limit) {
      throw apiError(403, {
        code: "quota_exceeded",
        message_key: "entitlement.limit_reached",
        details: { max: limit },
      });
    }
    gateway.placement = "bound";
    gateway.tenantId = tenantId;
    gateway.boundAt = this.now();
    gateway.health = "unknown";
    tenant.gateways += 1;
    this.record({
      type: "assets.gateway_bound",
      producer: "assets",
      tenantId,
      aggregateType: "gateway",
      aggregateId: hardwareId,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { binding_id: fakeUuid(), hardware_id: hardwareId },
    });
  }

  private release_(gateway: GatewayRecord, placement: Placement, type: string, payload: Record<string, unknown>) {
    const tenant = gateway.tenantId ? this.tenants.get(gateway.tenantId) : undefined;
    if (tenant) tenant.gateways -= 1;
    const tenantId = gateway.tenantId;
    gateway.placement = placement;
    gateway.tenantId = null;
    gateway.boundAt = null;
    gateway.health = "unknown";
    gateway.backlog = null;
    gateway.revoked = true;
    this.record({
      type,
      producer: "assets",
      tenantId,
      aggregateType: "gateway",
      aggregateId: gateway.hardwareId,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { hardware_id: gateway.hardwareId, ...payload },
    });
  }

  async unbindGateway(hardwareId: string): Promise<void> {
    await this.enter("unbindGateway", hardwareId);
    const gateway = this.gateway(hardwareId);
    if (gateway.placement !== "bound") throw conflict("error.conflict");
    this.release_(gateway, "pool", "assets.gateway_unbound", {});
  }

  async sendForRepair(hardwareId: string, input: { note: string }): Promise<void> {
    await this.enter("sendForRepair", hardwareId);
    const gateway = this.gateway(hardwareId);
    if (gateway.placement !== "bound") throw conflict("error.conflict");
    this.release_(gateway, "repair", "assets.gateway_sent_for_repair", { note: input.note });
  }

  async revokeGatewayCertificates(hardwareId: string, input: { reason: RevocationReason }): Promise<void> {
    await this.enter("revokeGatewayCertificates", hardwareId);
    const gateway = this.gateway(hardwareId);
    if (gateway.revoked) throw conflict("gateway_revocation.no_valid_certificate");
    gateway.revoked = true;
    gateway.health = "offline";
    this.record({
      type: "certs.gateway_certificates_revoked",
      producer: "certs",
      tenantId: gateway.tenantId,
      aggregateType: "gateway_registration",
      aggregateId: hardwareId,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { hardware_id: hardwareId, reason: input.reason },
    });
  }

  async decommissionGateway(hardwareId: string): Promise<void> {
    await this.enter("decommissionGateway", hardwareId);
    const gateway = this.gateway(hardwareId);
    if (gateway.placement === "bound") {
      this.release_(gateway, "pool", "assets.gateway_unbound", {});
    }
    this.gateways.delete(hardwareId);
    this.decommissioned.add(hardwareId);
    this.record({
      type: "certs.gateway_decommissioned",
      producer: "certs",
      tenantId: null,
      aggregateType: "gateway_registration",
      aggregateId: hardwareId,
      actor: { kind: "user", userId: ADMIN_USER_ID },
      payload: { hardware_id: hardwareId },
    });
  }

  async listProvisioning(query: ProvisioningQuery): Promise<ProvisioningPage> {
    await this.enter("listProvisioning");
    const from = query.from ? Date.parse(query.from) : Number.NEGATIVE_INFINITY;
    const to = query.to ? Date.parse(query.to) : Number.POSITIVE_INFINITY;
    const all = [...this.registrations]
      .map((entry): ProvisioningEntry => {
        const gateway = this.gateways.get(entry.hardwareId);
        const tenant = gateway?.tenantId ? this.tenants.get(gateway.tenantId) : undefined;
        return {
          ...entry,
          placement: this.decommissioned.has(entry.hardwareId)
            ? "decommissioned"
            : (gateway?.placement ?? entry.placement),
          tenantName: tenant?.name ?? null,
        };
      })
      .filter((entry) => !query.workstation || entry.workstation === query.workstation)
      .filter((entry) => Date.parse(entry.occurredAt) >= from && Date.parse(entry.occurredAt) < to)
      .sort((a, b) => Date.parse(b.occurredAt) - Date.parse(a.occurredAt));
    const start = query.cursor ? Number(query.cursor) : 0;
    return {
      entries: all.slice(start, start + this.pageSize),
      nextCursor: start + this.pageSize < all.length ? String(start + this.pageSize) : null,
      workstations: [...new Set(this.registrations.map((entry) => entry.workstation))].sort(),
    };
  }

  /** The demo data of the designed normal state (docs/design/platform-admin/): six tenants, five gateways. */
  private seedNormal(): void {
    const now = this.now();
    const ago = (ms: number) => new Date(now.getTime() - ms);
    const day = 24 * 60 * MINUTE;
    this.users.set("00000000-0000-4000-8000-0000000000a2", "ops@example.test");

    const a = this.addTenant("Demo Tenant A", {
      plan: "business",
      createdAt: ago(23 * day),
      stateChangedAt: ago(23 * day),
      gateways: 3,
      alertRules: 12,
      aiUsedUsd: 12.4,
      aiExtraUsd: 20,
    });
    for (const [email, role] of [
      ["owner@example.test", "admin"],
      ["pm@example.test", "viewer"],
      ["line1@example.test", "editor"],
      ["line2@example.test", "editor"],
      ["viewer@example.test", "viewer"],
      ["qa@example.test", "viewer"],
    ] as const) {
      this.addMember(a, email, role);
    }

    const north = this.addTenant("North Plant Demo", {
      plan: "enterprise",
      state: "suspended",
      reason: "security_or_abuse",
      stateVersion: 4,
      appliedVersion: 3,
      stateChangedAt: ago(2 * MINUTE + 14_000),
      changedByUserId: ADMIN_USER_ID,
      gateways: 1,
      alertRules: 30,
      aiUsedUsd: 88.1,
      createdAt: ago(40 * day),
    });
    for (let index = 0; index < 18; index += 1) this.addMember(north, `north${index + 1}@example.test`, "viewer");

    const south = this.addTenant("Nangang Test Tenant", {
      plan: "starter",
      state: "suspended",
      reason: "non_payment",
      stateVersion: 2,
      appliedVersion: 2,
      stateChangedAt: ago(5 * day),
      changedByUserId: "00000000-0000-4000-8000-0000000000a2",
      gateways: 1,
      createdAt: ago(60 * day),
    });
    this.addMember(south, "south1@example.test", "admin");
    this.addMember(south, "south2@example.test", "viewer");

    const old = this.addTenant("Old Contract Demo", {
      plan: "business",
      state: "suspended",
      reason: "contract_terminated",
      stateVersion: 2,
      appliedVersion: 2,
      stateChangedAt: ago(35 * day),
      changedByUserId: "00000000-0000-4000-8000-0000000000a2",
      createdAt: ago(200 * day),
    });
    for (let index = 0; index < 4; index += 1) this.addMember(old, `old${index + 1}@example.test`, "viewer");

    const long = this.addTenant(
      "A very long tenant name used to check how the page wraps, Demo Manufacturing Co., Ltd., second plant",
      {
        plan: "business",
        gateways: 5,
        alertRules: 8,
        aiUsedUsd: 41.72,
        createdAt: ago(90 * day),
      },
    );
    for (let index = 0; index < 31; index += 1) this.addMember(long, `plant${index + 1}@example.test`, "viewer");

    const acme = this.addTenant("Acme Labs Test", {
      plan: "starter",
      gateways: 1,
      aiUsedUsd: 10.31,
      createdAt: ago(12 * day),
    });
    this.addMember(acme, "lab@example.test", "admin");

    const gateway = (id: string, init: Partial<GatewayRecord>) => this.addGateway({ hardwareId: id, ...init });
    gateway("TIG-24A0-0007", {
      placement: "bound",
      tenantId: a.id,
      health: "online",
      lastReportAt: ago(12_000),
      boundAt: ago(15 * day),
      registeredAt: ago(21 * day),
    });
    gateway("TIG-24A0-0011", {
      placement: "bound",
      tenantId: a.id,
      health: "degraded",
      backlog: 4210,
      lastReportAt: ago(48_000),
      boundAt: ago(14 * day),
      registeredAt: ago(21 * day),
    });
    gateway("TIG-24A0-0019", {
      placement: "bound",
      tenantId: a.id,
      health: "offline",
      lastReportAt: ago(3 * 60 * MINUTE),
      boundAt: ago(10 * day),
      registeredAt: ago(20 * day),
      workstation: "WS-02",
    });
    gateway("TIG-24A0-0023", {
      placement: "bound",
      tenantId: north.id,
      health: "offline",
      lastReportAt: ago(3 * 60 * MINUTE),
      boundAt: ago(9 * day),
      registeredAt: ago(19 * day),
      workstation: "WS-02",
    });
    gateway("TIG-24A0-0031", { workstation: "WS-02", registeredAt: ago(5 * day) });
    gateway("TIG-24A0-0002", { placement: "repair", registeredAt: ago(24 * day), lastReportAt: ago(10 * day) });

    // The operation log: what the screens' design shows in the recent-operations card.
    const log = (ms: number, event: Parameters<FakePlatform["record"]>[0]) =>
      this.record({ ...event, occurredAt: ago(ms) });
    const admin: LoggedActor = { kind: "user", userId: ADMIN_USER_ID };
    log(3 * day, {
      type: "identity.quota_changed",
      producer: "identity",
      tenantId: a.id,
      aggregateType: "tenant_quota",
      aggregateId: a.id,
      actor: admin,
      payload: { ai_extra_usd: { from: 0, to: 20 }, reason: "More headroom before a customer demo" },
    });
    log(7 * day, {
      type: "identity.member_role_changed",
      producer: "identity",
      tenantId: a.id,
      aggregateType: "membership",
      aggregateId: `${a.id}:pm`,
      actor: admin,
      payload: { user_id: a.members[1]?.userId, previous_role: "viewer", role: "admin" },
    });
    log(15 * day, {
      type: "assets.gateway_bound",
      producer: "assets",
      tenantId: a.id,
      aggregateType: "gateway",
      aggregateId: "TIG-24A0-0007",
      actor: admin,
      payload: { binding_id: fakeUuid(), hardware_id: "TIG-24A0-0007" },
    });
    log(2 * MINUTE + 14_000, {
      type: "identity.tenant_state_changed",
      producer: "identity",
      tenantId: north.id,
      aggregateType: "tenant",
      aggregateId: north.id,
      actor: admin,
      payload: { state: "suspended", reason: "security_or_abuse", state_version: 4 },
    });
    log(5 * day, {
      type: "identity.tenant_state_changed",
      producer: "identity",
      tenantId: south.id,
      aggregateType: "tenant",
      aggregateId: south.id,
      actor: { kind: "user", userId: "00000000-0000-4000-8000-0000000000a2" },
      payload: { state: "suspended", reason: "non_payment", state_version: 2 },
    });
    log(5 * day - MINUTE, {
      type: "certs.tenant_state_applied",
      producer: "certs",
      tenantId: south.id,
      aggregateType: "tenant_state",
      aggregateId: south.id,
      actor: { kind: "service", service: "certs" },
      payload: { state_version: 2 },
    });
    log(2 * day, {
      type: "identity.ai_model_role_changed",
      producer: "identity",
      tenantId: null,
      aggregateType: "ai_model_role",
      aggregateId: "main",
      actor: admin,
      payload: { role: "main", action: "set", version: 14, models: ["claude-sonnet-5-5", "claude-opus-5-5"] },
    });
    log(day, {
      type: "assets.something_new",
      producer: "assets",
      tenantId: null,
      aggregateType: "thing",
      aggregateId: "x",
      actor: null,
      payload: { note: "an event type this build does not know" },
    });
    this.events.sort((x, y) => Date.parse(x.occurredAt) - Date.parse(y.occurredAt));
  }
}

/** The shape the UI preview reads (docs/design/ui-preview, Laysi/trustiot-v2#1065). */
export const preview = {
  name: "Platform admin",
  provider: (seed: FakePlatformSeed): ComponentType<{ children: ReactNode }> => {
    return function PlatformPreviewProvider({ children }) {
      const fake = useMemo(() => {
        const server = new FakePlatform(seed);
        // A suspension is confirmed a few seconds later, so the "processing" state can be watched.
        server.autoConfirmMs = 8000;
        return server;
      }, []);
      useEffect(() => () => fake.dispose(), [fake]);
      return createElement(PlatformApiProvider, { api: fake, children });
    };
  },
};
