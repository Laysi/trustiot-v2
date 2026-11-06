/**
 * The alerts pages' paths (docs/design/ms4-ms5/design.md, 「深連結」): the alerts and notification
 * center, one rule's page, and a notification in the list.
 */
export const ALERTS_PATH = "/alerts";

export type AlertsTab = "notifications" | "rules";

export function alertsPath(tab: AlertsTab = "notifications"): string {
  return tab === "notifications" ? ALERTS_PATH : `${ALERTS_PATH}?tab=rules`;
}

/** The notification list scrolled to `notificationId` and showing it marked. */
export function notificationPath(notificationId: string): string {
  return `${ALERTS_PATH}/notifications/${encodeURIComponent(notificationId)}`;
}

/**
 * One rule's page. `version` opens that version in the history with its differences; `action:
 * "disable"` opens the disable confirmation (the link of the notification Email, which never
 * disables anything itself); `section: "recent"` is the recent triggers, where a notification leads.
 */
export function rulePath(
  ruleId: string,
  options: { version?: number; action?: "disable"; section?: "recent" | "versions" } = {},
): string {
  const query = new URLSearchParams();
  if (options.version !== undefined) query.set("version", String(options.version));
  if (options.action !== undefined) query.set("action", options.action);
  const search = query.size > 0 ? `?${query}` : "";
  const hash = options.section !== undefined ? `#${options.section}` : "";
  return `${ALERTS_PATH}/rules/${encodeURIComponent(ruleId)}${search}${hash}`;
}
