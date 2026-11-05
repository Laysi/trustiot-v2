import { type ComponentType, createElement, type ReactNode } from "react";

import { AlertsApiProvider } from "../api/pending/AlertsApiProvider.tsx";
import type {
  AiPause,
  AlertsApi,
  AlertsOverview,
  Backtest,
  FeedItem,
  Notification,
  NotificationPage,
  NotificationQuery,
  RuleDetail,
  RuleOrigin,
  RuleSummary,
  RuleVersion,
} from "../api/pending/alerts.ts";
import { ApiError, type ErrorCode, parseErrorBody } from "../api/errors.ts";

/**
 * The alerts API (api/pending/alerts.ts) held in memory, for the UI preview and the tests. It
 * follows the rules the design and the two backend issues state:
 *
 * - A notification list is newest first; "unread only" leaves out read notifications (the read
 *   state is the caller's, decision Q4); a limit notice stays in the list whatever the read
 *   filter, and goes with its rule when the rule is filtered.
 * - Only an active rule can be disabled and only a disabled one enabled (409 `conflict`);
 *   disabling keeps the reason (decision Q3) and marks the rule's notifications.
 * - The pause of every AI rule turns off the AI-made rules only (decision Q1); the member-made
 *   ones keep running. The rule list does not change when it is paused: the page derives the
 *   state from the pause, as the server's list does not rewrite each rule.
 *
 * Every call is recorded. `failNext` makes the next call to an endpoint fail, `hold` keeps calls
 * to an endpoint waiting until `release`.
 */

export type AlertsEndpoint =
  | "overview"
  | "notifications"
  | "notification"
  | "markRead"
  | "markAllRead"
  | "rules"
  | "rule"
  | "disable"
  | "enable"
  | "pause"
  | "resume";

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

export function fakeAlertsError(status: number, code: ErrorCode) {
  return new ApiError(status, parseErrorBody({ code, message_key: `error.${code}` }));
}

export interface AlertsCall {
  endpoint: AlertsEndpoint;
  args: unknown[];
}

let sequence = 0;
function nextId(prefix: string): string {
  sequence += 1;
  return `${prefix}-${sequence.toString().padStart(4, "0")}`;
}

export class FakeAlertsServer implements AlertsApi {
  readonly rules = new Map<string, RuleDetail>();
  /** Newest first. */
  feed: FeedItem[] = [];
  pause: AiPause = { paused: false };
  planAllows = true;
  /** The member the calls act for, in the pause and disable records. */
  actor = "Wang Xiaoming";
  /** The list's page size. */
  pageSize = 50;
  now: () => number = () => Date.now();
  readonly calls: AlertsCall[] = [];
  private readonly failures = new Map<AlertsEndpoint, Error[]>();
  private readonly holds = new Map<AlertsEndpoint, (() => void)[]>();
  private holding = new Set<AlertsEndpoint>();

  failNext(endpoint: AlertsEndpoint, error: Error = fakeAlertsError(503, "unavailable")) {
    this.failures.set(endpoint, [...(this.failures.get(endpoint) ?? []), error]);
  }

  /** Keeps every call to `endpoint` waiting until `release()`. */
  hold(endpoint: AlertsEndpoint) {
    this.holding.add(endpoint);
  }

  release(endpoint: AlertsEndpoint) {
    this.holding.delete(endpoint);
    for (const resume of this.holds.get(endpoint) ?? []) resume();
    this.holds.delete(endpoint);
  }

  callsTo(endpoint: AlertsEndpoint): AlertsCall[] {
    return this.calls.filter((call) => call.endpoint === endpoint);
  }

  private async enter(endpoint: AlertsEndpoint, args: unknown[], signal?: AbortSignal) {
    this.calls.push({ endpoint, args });
    if (this.holding.has(endpoint)) {
      await new Promise<void>((resolve) => {
        this.holds.set(endpoint, [...(this.holds.get(endpoint) ?? []), resolve]);
      });
    } else {
      await Promise.resolve();
    }
    signal?.throwIfAborted();
    const failure = this.failures.get(endpoint)?.shift();
    if (failure) throw failure;
  }

  /** Adds a rule and returns it; `versions` are newest first, the first one is the current. */
  addRule(
    rule: Partial<RuleDetail> & { name: string; versions?: RuleVersion[] },
  ): RuleDetail {
    const id = rule.id ?? nextId("rule");
    const origin: RuleOrigin = rule.origin ?? { kind: "ai" };
    const versions = rule.versions ?? [
      {
        version: 1,
        author: origin,
        created_at: new Date(this.now() - 3 * DAY).toISOString(),
        summary: "First version.",
        script: "if point(\"x\").last() > 1.0 { alert(#{ value: 1 }); }",
        backtest: fakeBacktest(this.now(), { passed: rule.status !== "backtest_failed" }),
      },
    ];
    const current = versions[0] as RuleVersion;
    const detail: RuleDetail = {
      description: "Notifies when the value stays beyond its threshold.",
      cooldown_minutes: 15,
      hourly_limit: 4,
      channels: { in_app: true, email_recipients: 0, webhook_names: [] },
      scope: [],
      recent_triggers: [],
      applies_to: [],
      last_triggered_at: null,
      status: "active",
      ...rule,
      id,
      origin,
      version: current.version,
      script: current.script,
      versions,
    };
    this.rules.set(id, detail);
    return detail;
  }

  addNotification(item: Partial<Notification> & { summary: string; rule_id: string }): Notification {
    const rule = this.rules.get(item.rule_id);
    const notification: Notification = {
      type: "notification",
      id: nextId("ntf"),
      rule_name: rule?.name ?? "Rule",
      rule_origin: rule?.origin ?? { kind: "ai" },
      rule_disabled: rule?.status === "disabled",
      device_id: "dev-1",
      device_name: "Compressor A2",
      triggered_at: new Date(this.now() - 12 * MINUTE).toISOString(),
      merged_count: 1,
      read: false,
      kind: "threshold",
      ...item,
    };
    this.feed.unshift(notification);
    return notification;
  }

  private notifications(): Notification[] {
    return this.feed.filter((item): item is Notification => item.type === "notification");
  }

  private unreadCount(): number {
    return this.notifications().filter((item) => !item.read).length;
  }

  async getOverview(signal?: AbortSignal): Promise<AlertsOverview> {
    await this.enter("overview", [], signal);
    const all = [...this.rules.values()];
    return {
      plan_allows: this.planAllows,
      unread_count: this.unreadCount(),
      rule_count: all.length,
      ai_rule_count: all.filter((rule) => rule.origin.kind === "ai" && rule.status === "active").length,
      member_rule_count: all.filter((rule) => rule.origin.kind === "member" && rule.status === "active").length,
      pause: { ...this.pause },
    };
  }

  async listNotifications(query: NotificationQuery, signal?: AbortSignal): Promise<NotificationPage> {
    await this.enter("notifications", [query], signal);
    const matches = this.feed.filter((item) => {
      if (item.type === "limit_reached") return (query.ruleId ?? item.rule_id) === item.rule_id && !query.deviceId;
      return (
        (!query.unreadOnly || !item.read) &&
        (query.deviceId === undefined || item.device_id === query.deviceId) &&
        (query.ruleId === undefined || item.rule_id === query.ruleId)
      );
    });
    const start = query.cursor === undefined ? 0 : Number(query.cursor);
    const items = matches.slice(start, start + this.pageSize);
    const notifications = this.notifications();
    return {
      items,
      ...(start + this.pageSize < matches.length ? { next_cursor: String(start + this.pageSize) } : {}),
      devices: [...new Map(notifications.map((item) => [item.device_id, item.device_name])).entries()].map(
        ([id, name]) => ({ id, name }),
      ),
      rules: [...new Map(notifications.map((item) => [item.rule_id, item.rule_name])).entries()].map(
        ([id, name]) => ({ id, name }),
      ),
    };
  }

  async getNotification(id: string, signal?: AbortSignal): Promise<Notification> {
    await this.enter("notification", [id], signal);
    const found = this.notifications().find((item) => item.id === id);
    if (!found) throw fakeAlertsError(404, "not_found");
    return { ...found };
  }

  async markRead(id: string): Promise<void> {
    await this.enter("markRead", [id]);
    const found = this.notifications().find((item) => item.id === id);
    if (!found) throw fakeAlertsError(404, "not_found");
    found.read = true;
  }

  async markAllRead(): Promise<void> {
    await this.enter("markAllRead", []);
    for (const item of this.notifications()) item.read = true;
  }

  async listRules(signal?: AbortSignal): Promise<RuleSummary[]> {
    await this.enter("rules", [], signal);
    return [...this.rules.values()].map((summary) => ({
      id: summary.id,
      name: summary.name,
      origin: summary.origin,
      status: summary.status,
      ...(summary.disabled ? { disabled: summary.disabled } : {}),
      ...(summary.backtest_failure ? { backtest_failure: summary.backtest_failure } : {}),
      applies_to: summary.applies_to,
      last_triggered_at: summary.last_triggered_at,
      version: summary.version,
    }));
  }

  async getRule(id: string, signal?: AbortSignal): Promise<RuleDetail> {
    await this.enter("rule", [id], signal);
    const rule = this.rules.get(id);
    if (!rule) throw fakeAlertsError(404, "not_found");
    return structuredClone(rule);
  }

  async disableRule(id: string, reason?: string): Promise<void> {
    await this.enter("disable", [id, reason]);
    const rule = this.rules.get(id);
    if (!rule) throw fakeAlertsError(404, "not_found");
    if (rule.status !== "active") throw fakeAlertsError(409, "conflict");
    rule.status = "disabled";
    rule.disabled = {
      by: this.actor,
      at: new Date(this.now()).toISOString(),
      ...(reason !== undefined && reason !== "" ? { reason } : {}),
    };
    for (const item of this.notifications()) if (item.rule_id === id) item.rule_disabled = true;
  }

  async enableRule(id: string): Promise<void> {
    await this.enter("enable", [id]);
    const rule = this.rules.get(id);
    if (!rule) throw fakeAlertsError(404, "not_found");
    if (rule.status !== "disabled") throw fakeAlertsError(409, "conflict");
    rule.status = "active";
    delete rule.disabled;
    for (const item of this.notifications()) if (item.rule_id === id) item.rule_disabled = false;
  }

  async pauseAiRules(): Promise<void> {
    await this.enter("pause", []);
    this.pause = { paused: true, by: this.actor, at: new Date(this.now()).toISOString() };
  }

  async resumeAiRules(): Promise<void> {
    await this.enter("resume", []);
    this.pause = { paused: false };
  }
}

/** A backtest of the 14 days before `now`, with `triggers` firing on the given days ago. */
export function fakeBacktest(
  now: number,
  options: { passed?: boolean; triggerDaysAgo?: number[]; historyDays?: number; limit?: number; cases?: Backtest["cases"] } = {},
): Backtest {
  const passed = options.passed ?? true;
  const days = options.triggerDaysAgo ?? [11, 8, 8, 5, 1];
  const counts = new Map<number, number>();
  for (const day of days) counts.set(day, (counts.get(day) ?? 0) + 1);
  const to = new Date(now);
  const daily = Array.from({ length: 14 }, (_, index) => {
    const ago = 13 - index;
    const date = new Date(now - ago * DAY).toISOString().slice(0, 10);
    return { date, count: counts.get(ago + 1) ?? 0 };
  });
  return {
    passed,
    from: new Date(now - 14 * DAY).toISOString(),
    to: to.toISOString(),
    history_days: options.historyDays ?? 14,
    syntax_ok: true,
    execution_ok: true,
    trigger_count: days.length,
    trigger_limit: options.limit ?? 50,
    cases: options.cases ?? [
      { source: "history", at: new Date(now - 9 * DAY).toISOString(), triggered: true },
      { source: "synthetic", triggered: passed },
    ],
    daily_counts: daily,
    triggers: days.map((ago, index) => ({
      at: new Date(now - ago * DAY - index * HOUR).toISOString(),
      device_name: "Compressor A2",
      summary: "Outlet pressure 5.8 bar, below 6.2 bar",
    })),
  };
}

/** The demo data of the design (docs/design/ms4-ms5/alerts.html): fictional factory, rules and people. */
export function seedAlerts(server: FakeAlertsServer, seed: "normal" | "empty" = "normal") {
  if (seed === "empty") return server;
  const now = server.now();
  const iso = (ago: number) => new Date(now - ago).toISOString();
  const script = (threshold: string, extra = "") =>
    `let p = point("outlet_pressure").window(5m);\n${extra}if p.all(|v| v < ${threshold}) {\n  alert(#{ value: p.last(), threshold: ${threshold} });\n}`;

  const pressure = server.addRule({
    id: "rule-pressure",
    name: "Compressor outlet pressure too low",
    description: "Notifies when the outlet pressure stays below 6.2 bar for 5 minutes; nothing is judged while the compressor is stopped (current below 2 A).",
    channels: { in_app: true, email_recipients: 0, webhook_names: ["Maintenance system"] },
    scope: [
      { device_id: "dev-a1", device_name: "Compressor A1", point: "outlet_pressure" },
      { device_id: "dev-a2", device_name: "Compressor A2", point: "outlet_pressure" },
    ],
    applies_to: ["Compressor A1", "Compressor A2"],
    last_triggered_at: iso(12 * MINUTE),
    recent_triggers: [
      { at: iso(12 * MINUTE), device_id: "dev-a2", device_name: "Compressor A2", summary: "5.8 bar" },
      { at: iso(2 * DAY), device_id: "dev-a2", device_name: "Compressor A2", summary: "6.0 bar" },
    ],
    versions: [
      {
        version: 3,
        author: { kind: "ai" },
        created_at: iso(5 * DAY),
        summary: "Threshold 6.0 to 6.2 bar; no judgment while the compressor is stopped. Reason: v2 raised 9 false alarms while it was stopped.",
        script: script("6.2", 'if point("motor_current").last() < 2.0 { return; }\n'),
        backtest: fakeBacktest(now, { triggerDaysAgo: [11, 8, 8, 5, 1, 1] }),
      },
      {
        version: 2,
        author: { kind: "ai" },
        created_at: iso(6 * DAY),
        summary: "Judging time 2 to 5 minutes, fewer false alarms at shift change.",
        script: script("6.0"),
        backtest: fakeBacktest(now, { triggerDaysAgo: [11, 8, 5] }),
      },
      {
        version: 1,
        author: { kind: "ai" },
        created_at: iso(7 * DAY),
        summary: "The agent learned this rule from the pressure drop on the 21st.",
        script: script("6.0").replace("5m", "2m"),
        backtest: fakeBacktest(now, { historyDays: 9, triggerDaysAgo: [8, 8] }),
      },
    ],
  });
  const standby = server.addRule({
    id: "rule-standby",
    name: "Press standby power too high",
    applies_to: ["Press A"],
    scope: [{ device_id: "dev-pm1", device_name: "Meter PM-01", point: "active_power" }],
    last_triggered_at: iso(2 * HOUR),
    channels: { in_app: true, email_recipients: 2, webhook_names: [] },
  });
  const tower = server.addRule({
    id: "rule-tower",
    name: "Cooling tower temperature limit",
    origin: { kind: "member", name: "Chen Meiling" },
    applies_to: ["Tower thermometer TT-1"],
    scope: [{ device_id: "dev-tt1", device_name: "Tower thermometer TT-1", point: "temperature" }],
    last_triggered_at: iso(4 * HOUR),
  });
  const offline = server.addRule({
    id: "rule-offline",
    name: "Injection machine offline",
    status: "disabled",
    disabled: { by: "Wang Xiaoming", at: iso(16 * HOUR), reason: "Planned maintenance." },
    applies_to: ["Injection machine 3"],
    last_triggered_at: iso(17 * HOUR),
  });
  const vibration = server.addRule({
    id: "rule-vibration",
    name: "Feeder vibration spike",
    status: "backtest_failed",
    backtest_failure: "expected_case",
    applies_to: ["Vibration sensor VB-03"],
    versions: [
      {
        version: 1,
        author: { kind: "ai" },
        created_at: iso(DAY),
        summary: "Looks for vibration spikes above the usual level.",
        script: 'if point("vibration").last() > 9.0 { alert(#{ value: 9.0 }); }',
        backtest: {
          ...fakeBacktest(now, { passed: false, triggerDaysAgo: [], cases: [{ source: "synthetic", triggered: false }] }),
          trigger_count: 0,
        },
      },
    ],
  });
  server.addRule({ name: "Conveyor motor temperature high", applies_to: ["Conveyor M2"] });

  server.addNotification({
    rule_id: offline.id,
    device_id: "dev-im3",
    device_name: "Injection machine 3",
    kind: "offline",
    summary: "Injection machine 3 has been offline for over 30 minutes",
    triggered_at: iso(17 * HOUR),
    read: true,
  });
  server.addNotification({
    rule_id: tower.id,
    device_id: "dev-tt1",
    device_name: "Tower thermometer TT-1",
    summary: "Cooling tower outlet temperature 35.4 °C, above 35 °C",
    triggered_at: iso(4 * HOUR),
  });
  server.feed.unshift({
    type: "limit_reached",
    id: nextId("lim"),
    rule_id: pressure.id,
    rule_name: pressure.name,
    count: 4,
    until: iso(-40 * MINUTE),
    at: iso(12 * MINUTE),
  });
  server.addNotification({
    rule_id: standby.id,
    device_id: "dev-pm1",
    device_name: "Meter PM-01",
    part_number: "BRK-1023",
    summary: "Press A standby power 4.1 kW, 38% above the same hours of the past 14 days",
    triggered_at: iso(2 * HOUR),
  });
  server.addNotification({
    rule_id: pressure.id,
    summary: "Compressor A2 outlet pressure 5.8 bar, below 6.2 bar for 6 minutes",
    merged_count: 3,
  });
  void vibration;
  return server;
}

/** What the UI preview's toolbar reads (frontend/src/ui-preview): this fake, seeded `normal` or `empty`. */
export const preview = {
  name: "Alerts",
  provider: (seed: "normal" | "empty"): ComponentType<{ children: ReactNode }> => {
    const api = seedAlerts(new FakeAlertsServer(), seed);
    return ({ children }) => createElement(AlertsApiProvider, { api, children });
  },
};
