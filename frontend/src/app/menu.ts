import { CpuIcon, LayoutDashboardIcon, type LucideIcon, MessagesSquareIcon, SettingsIcon } from "lucide-react";
import { createContext, useContext } from "react";

import type { MessageId } from "../i18n/messages.ts";
import type { SessionInfo } from "../session/session.ts";
import {
  CONVERSATIONS_PATH,
  DASHBOARDS_PATH,
  DEVICES_PATH,
  isConversationPath,
  SETTINGS_PATH,
  SHARED_CONVERSATIONS_PATH,
} from "./paths.ts";

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
  /** Unique among the entries; the UI preview adds entries of its own (frontend/README.md, 「UI 預覽」). */
  id: string;
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

/** Whether `pathname` is `path` itself, with a trailing slash or not, in any case (as the router matches). */
function exactly(path: string, pathname: string): boolean {
  return pathname.toLowerCase().replace(/\/+$/, "") === path;
}

export const MENU_ITEMS: readonly MenuItem[] = [
  {
    id: "conversations",
    group: 1,
    label: "shell.menu.conversations",
    icon: MessagesSquareIcon,
    path: CONVERSATIONS_PATH,
    // The history, and a conversation another member shared (opened from its "Shared with me"
    // tab): a conversation of the user's own is a conversation page, which no entry stands for.
    covers: (pathname) =>
      exactly(CONVERSATIONS_PATH, pathname) || under(SHARED_CONVERSATIONS_PATH, pathname.toLowerCase()),
    visibleTo: (session) => session.tenants.length > 0,
  },
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
    id: "devices",
    group: 2,
    label: "shell.menu.devices",
    icon: CpuIcon,
    path: DEVICES_PATH,
    covers: (pathname) => under(DEVICES_PATH, pathname),
    // The devices of the tenant the session works in.
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

/**
 * The entries the shell shows: `MENU_ITEMS` in the app. Only the UI preview provides another list
 * (`MENU_ITEMS` plus the entries of pages whose backend does not exist yet).
 */
export const MenuItemsContext = createContext<readonly MenuItem[]>(MENU_ITEMS);

export function useMenuItems(): readonly MenuItem[] {
  return useContext(MenuItemsContext);
}

/** The entries `session` may use, grouped in order; a group without an entry is not there. */
export function menuGroups(session: SessionInfo | null, items: readonly MenuItem[] = MENU_ITEMS): MenuItem[][] {
  if (session === null) return [];
  const groups = new Map<number, MenuItem[]>();
  for (const item of items) {
    if (!item.visibleTo(session)) continue;
    groups.set(item.group, [...(groups.get(item.group) ?? []), item]);
  }
  return [...groups.entries()].sort(([a], [b]) => a - b).map(([, items]) => items);
}

/** The entry whose page `pathname` is, or `null` (the home page and the conversations have none). */
export function currentMenuItem(pathname: string, items: readonly MenuItem[] = MENU_ITEMS): MenuItem["id"] | null {
  return items.find((item) => item.covers(pathname))?.id ?? null;
}

/**
 * Whether `pathname` is a secondary page, one that is not a conversation: it has the way back to
 * the conversation (app-shell.md, 「原則」). The conversation pages are the conversation home and
 * each conversation's page; the history of conversations is a secondary page.
 */
export function isSecondaryPage(pathname: string): boolean {
  return pathname !== "/" && !isConversationPath(pathname);
}
