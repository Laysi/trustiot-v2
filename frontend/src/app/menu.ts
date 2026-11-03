import { LayoutDashboardIcon, type LucideIcon, SettingsIcon } from "lucide-react";

import type { MessageId } from "../i18n/messages.ts";
import type { SessionInfo } from "../session/session.ts";
import { DASHBOARDS_PATH, SETTINGS_PATH } from "./paths.ts";

/**
 * One entry of the menu in the top bar (docs/design/app-shell.md, 「選單」). The menu, the phone's
 * menu and the check that the current page's entry is marked all read this list, so an entry has
 * one definition: where it leads, who sees it, and which pages it stands for.
 *
 * Only pages that exist are listed here; a page that goes live adds its entry (the group numbers
 * are the ones of the table in app-shell.md, and a group with no entry shown disappears with its
 * divider). The entries still to come are in that table.
 */
export interface MenuItem {
  id: "dashboards" | "settings";
  /** The table's group number: entries of one group sit together, groups are divided by a line. */
  group: number;
  label: MessageId;
  icon: LucideIcon;
  path: string;
  /** Whether the page is this entry's or below it (a dashboard's page belongs to the list). */
  covers(pathname: string): boolean;
  /** Whether this user may use the page: a role, a platform administrator, or a tenant to work in. */
  visibleTo(session: SessionInfo): boolean;
}

function under(path: string, pathname: string): boolean {
  return pathname === path || pathname.startsWith(`${path}/`);
}

export const MENU_ITEMS: readonly MenuItem[] = [
  {
    id: "dashboards",
    group: 1,
    label: "shell.menu.dashboards",
    icon: LayoutDashboardIcon,
    path: DASHBOARDS_PATH,
    covers: (pathname) => under(DASHBOARDS_PATH, pathname),
    // Every member of a tenant; a session in no tenant has no dashboards to list.
    visibleTo: (session) => session.tenants.length > 0,
  },
  {
    id: "settings",
    group: 4,
    label: "shell.menu.settings",
    icon: SettingsIcon,
    path: SETTINGS_PATH,
    covers: (pathname) => under(SETTINGS_PATH, pathname),
    visibleTo: () => true,
  },
];

/** The entries `session` may use, grouped in order; a group without an entry is not there. */
export function menuGroups(session: SessionInfo | null): MenuItem[][] {
  if (session === null) return [];
  const groups = new Map<number, MenuItem[]>();
  for (const item of MENU_ITEMS) {
    if (!item.visibleTo(session)) continue;
    groups.set(item.group, [...(groups.get(item.group) ?? []), item]);
  }
  return [...groups.entries()].sort(([a], [b]) => a - b).map(([, items]) => items);
}

/** The entry whose page `pathname` is, or `null` (the home page and the conversation have none). */
export function currentMenuItem(pathname: string): MenuItem["id"] | null {
  return MENU_ITEMS.find((item) => item.covers(pathname))?.id ?? null;
}

/**
 * Whether `pathname` is a secondary page, one that is not a conversation: it has the way back to
 * the conversation (app-shell.md, 「原則」). Until the conversation pages exist (Laysi/trustiot-v2#149)
 * the home page is the only conversation page.
 */
export function isSecondaryPage(pathname: string): boolean {
  return pathname !== "/";
}
