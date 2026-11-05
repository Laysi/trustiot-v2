import type { NotificationQuery } from "../../api/pending/alerts.ts";

// The alerts pages' query keys. Each starts with the active tenant's ID, so a cached answer is
// never shown for another tenant. Every write invalidates `alertsKey(tenant)`: a rule's state,
// the counts, the unread number and the lists depend on each other.

export const alertsKey = (tenant: string) => ["alerts", tenant] as const;
export const overviewKey = (tenant: string) => [...alertsKey(tenant), "overview"] as const;
export const notificationsKey = (tenant: string, query: Omit<NotificationQuery, "cursor">) =>
  [...alertsKey(tenant), "notifications", query.unreadOnly, query.deviceId ?? "", query.ruleId ?? ""] as const;
export const notificationKey = (tenant: string, id: string) => [...alertsKey(tenant), "notification", id] as const;
export const rulesKey = (tenant: string) => [...alertsKey(tenant), "rules"] as const;
export const ruleKey = (tenant: string, id: string) => [...alertsKey(tenant), "rule", id] as const;
