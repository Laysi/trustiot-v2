import type { MessageId } from "../../i18n/messages.ts";
import { aiSections } from "./ai/plugin.ts";
import {
  PLATFORM_AUDIT_PATH,
  PLATFORM_GATEWAYS_PATH,
  PLATFORM_PROVISIONING_PATH,
  PLATFORM_TENANTS_PATH,
} from "./paths.ts";

/**
 * The sections of the platform back office, in four groups (docs/design/platform-admin/design.md,
 * 「資訊架構」). The layout (`PlatformLayout`) draws them as a row of links on a wide screen and a
 * native select on a narrow one. A page of the back office plugs in by adding its section here or,
 * for the AI group, in `ai/plugin.ts` (`aiSections`); a group without a section is not drawn.
 */
export type PlatformGroupId = "tenants" | "gateways" | "records" | "ai";

export interface PlatformGroup {
  id: PlatformGroupId;
  label: MessageId;
}

export interface PlatformSection {
  id: string;
  group: PlatformGroupId;
  label: MessageId;
  path: string;
  /** Whether `pathname` is this section's page or below it (a tenant's page belongs to the tenants). */
  covers(pathname: string): boolean;
}

export const PLATFORM_GROUPS: readonly PlatformGroup[] = [
  { id: "tenants", label: "platform.group.tenants" },
  { id: "gateways", label: "platform.group.gateways" },
  { id: "records", label: "platform.group.records" },
  { id: "ai", label: "platform.group.ai" },
];

/** Whether `pathname` is `path` or below it. */
export function under(path: string, pathname: string): boolean {
  const normal = pathname.toLowerCase().replace(/\/+$/, "");
  return normal === path || normal.startsWith(`${path}/`);
}

const OWN_SECTIONS: readonly PlatformSection[] = [
  {
    id: "tenants",
    group: "tenants",
    label: "platform.section.tenants",
    path: PLATFORM_TENANTS_PATH,
    covers: (pathname) => under(PLATFORM_TENANTS_PATH, pathname),
  },
  {
    id: "gateways",
    group: "gateways",
    label: "platform.section.gateways",
    path: PLATFORM_GATEWAYS_PATH,
    covers: (pathname) => under(PLATFORM_GATEWAYS_PATH, pathname),
  },
  {
    id: "provisioning",
    group: "gateways",
    label: "platform.section.provisioning",
    path: PLATFORM_PROVISIONING_PATH,
    covers: (pathname) => under(PLATFORM_PROVISIONING_PATH, pathname),
  },
  {
    id: "audit",
    group: "records",
    label: "platform.section.audit",
    path: PLATFORM_AUDIT_PATH,
    covers: (pathname) => under(PLATFORM_AUDIT_PATH, pathname),
  },
];

/** Every section, grouped as the layout draws them: the groups that have one, each in order. */
export function platformSections(): { group: PlatformGroup; sections: PlatformSection[] }[] {
  const all = [...OWN_SECTIONS, ...aiSections];
  return PLATFORM_GROUPS.map((group) => ({
    group,
    sections: all.filter((section) => section.group === group.id),
  })).filter(({ sections }) => sections.length > 0);
}

/** The section whose page `pathname` is, or `null`. */
export function currentSection(pathname: string): PlatformSection | null {
  return [...OWN_SECTIONS, ...aiSections].find((section) => section.covers(pathname)) ?? null;
}
