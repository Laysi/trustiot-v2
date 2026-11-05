// Pending Laysi/trustiot-v2#169: replace with a module over the generated client when api/openapi.json has these endpoints.
//
// The alerts and notification center API the screens of Laysi/trustiot-v2#172 need. Neither backend
// issue (#168 backtest and guardrails, #169 notifications) defines its REST paths yet, so this
// interface takes its shape from their 範圍 and from docs/design/ms4-ms5/design.md, and the page
// talks to it through `AlertsApiProvider`. The assumptions are listed in the pull request.

/** Who started a rule: the cloud agent on its own ("AI 建立"), or a member who asked for it in a conversation (decision Q1). */
export type RuleOrigin = { kind: "ai" } | { kind: "member"; name: string };

/**
 * `active` judges and notifies, `disabled` was switched off by a person, `backtest_failed` never
 * ran (the guardrail of Laysi/trustiot-v2#168 refused it). The pause of every AI rule is not a
 * status of the rule: it is `AiPause`, and the page derives "paused" from both.
 */
export type RuleStatus = "active" | "disabled" | "backtest_failed";

export interface RuleDisabled {
  by: string;
  at: string;
  /** The optional reason given when disabling (decision Q3), the agent's input for its next adjustment. */
  reason?: string;
}

/** The four checks of Laysi/trustiot-v2#168. */
export interface BacktestCase {
  /** A stretch of real history, or one the backtest synthesized because the history had none. */
  source: "history" | "synthetic";
  /** When the stretch happened (history only). */
  at?: string;
  triggered: boolean;
}

export interface BacktestTrigger {
  at: string;
  device_name: string;
  summary: string;
}

export interface Backtest {
  passed: boolean;
  /** The window the rule was replayed on. */
  from: string;
  to: string;
  /** How many days of history the devices had inside the window; fewer than 14 means synthetic cases were added. */
  history_days: number;
  syntax_ok: boolean;
  execution_ok: boolean;
  trigger_count: number;
  trigger_limit: number;
  cases: BacktestCase[];
  /** One entry per day of the window, oldest first. */
  daily_counts: { date: string; count: number }[];
  triggers: BacktestTrigger[];
}

export interface RuleChannels {
  in_app: boolean;
  email_recipients: number;
  webhook_names: string[];
}

export interface RuleScopeItem {
  device_id: string;
  device_name: string;
  point: string;
}

/** A row of the rule list. */
export interface RuleSummary {
  id: string;
  name: string;
  origin: RuleOrigin;
  status: RuleStatus;
  disabled?: RuleDisabled;
  /** The check of Laysi/trustiot-v2#168 that refused the rule. */
  backtest_failure?: "syntax" | "execution" | "trigger_count" | "expected_case";
  /** Names of the devices it applies to, for the list. */
  applies_to: string[];
  last_triggered_at: string | null;
  version: number;
}

export interface RuleVersion {
  version: number;
  author: RuleOrigin;
  created_at: string;
  /** What changed and why, as the author wrote it. */
  summary: string;
  script: string;
  backtest: Backtest;
}

export interface RuleDetail extends RuleSummary {
  /** The plain-language description stored with the rule when it was made (never asked of an LLM on display). */
  description: string;
  script: string;
  cooldown_minutes: number;
  hourly_limit: number;
  channels: RuleChannels;
  scope: RuleScopeItem[];
  recent_triggers: { at: string; device_id: string; device_name: string; summary: string }[];
  /** Newest first. */
  versions: RuleVersion[];
}

export interface Notification {
  type: "notification";
  id: string;
  rule_id: string;
  rule_name: string;
  rule_origin: RuleOrigin;
  /** The rule has been disabled since: the notification stays, without a disable button. */
  rule_disabled: boolean;
  device_id: string;
  device_name: string;
  part_number?: string;
  /** What happened, with the value and the threshold, as the rule wrote it. */
  summary: string;
  triggered_at: string;
  /** Triggers merged into this one by the dedup of the same rule, device and window. */
  merged_count: number;
  /** Read by the signed-in user (decision Q4: per person). */
  read: boolean;
  /** The question the "Ask AI" button sends: why it fired, or why the device is offline. */
  kind: "threshold" | "offline";
}

/** The note the feed shows when a rule reached its hourly limit. */
export interface LimitNotice {
  type: "limit_reached";
  id: string;
  rule_id: string;
  rule_name: string;
  count: number;
  /** Triggers before this time are no longer notified. */
  until: string;
  at: string;
}

export type FeedItem = Notification | LimitNotice;

export interface NotificationQuery {
  unreadOnly: boolean;
  deviceId?: string;
  ruleId?: string;
  cursor?: string;
}

export interface NotificationPage {
  items: FeedItem[];
  next_cursor?: string;
  /** Devices and rules that have notifications, for the two filters. */
  devices: { id: string; name: string }[];
  rules: { id: string; name: string }[];
}

export interface AiPause {
  paused: boolean;
  by?: string;
  at?: string;
}

export interface AlertsOverview {
  /** Alerts are a Business plan feature (docs/product/plans.md); false shows the upgrade wall. */
  plan_allows: boolean;
  unread_count: number;
  rule_count: number;
  /** Rules the agent made that are on (or would be, without the pause): the number the strip and the pause dialog show. */
  ai_rule_count: number;
  member_rule_count: number;
  pause: AiPause;
}

export interface AlertsApi {
  getOverview(signal?: AbortSignal): Promise<AlertsOverview>;
  listNotifications(query: NotificationQuery, signal?: AbortSignal): Promise<NotificationPage>;
  /** One notification, 404 `not_found` when it does not exist (the deep link `/alerts/notifications/:id`). */
  getNotification(id: string, signal?: AbortSignal): Promise<Notification>;
  markRead(id: string): Promise<void>;
  markAllRead(): Promise<void>;
  listRules(signal?: AbortSignal): Promise<RuleSummary[]>;
  /** 404 `not_found` for a rule that does not exist. */
  getRule(id: string, signal?: AbortSignal): Promise<RuleDetail>;
  /** Editor or above (decision Q2); 403 `permission_denied` otherwise. */
  disableRule(id: string, reason?: string): Promise<void>;
  enableRule(id: string): Promise<void>;
  pauseAiRules(): Promise<void>;
  resumeAiRules(): Promise<void>;
}
