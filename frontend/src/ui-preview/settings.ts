import type { MemberRole } from "../session/session.ts";
import { PREVIEW_SEEDS, type PreviewSeed } from "./contract.ts";
import { SLOW_DELAYS_MS } from "./controls.ts";

/**
 * The toolbar's settings that the fakes are created with: the session's role, whether the user is
 * a platform administrator, and the seed. Changing one saves it in the tab's sessionStorage and
 * reloads the page, so every fake starts again from the same state (the data a page changed is
 * gone). The slow delay is kept too, for convenience; a failure is armed per page load.
 */

export interface PreviewSettings {
  role: MemberRole;
  platformAdmin: boolean;
  seed: PreviewSeed;
  slowMs: number;
}

export const ROLES: readonly MemberRole[] = ["admin", "editor", "viewer"];
export const DEFAULT_SETTINGS: PreviewSettings = { role: "admin", platformAdmin: false, seed: "normal", slowMs: 0 };
export const SETTINGS_KEY = "trustiot-ui-preview";

/** The settings `storage` holds, each field checked; the default for anything missing or wrong. */
export function loadSettings(storage: Storage | undefined): PreviewSettings {
  let saved: unknown;
  try {
    saved = JSON.parse(storage?.getItem(SETTINGS_KEY) ?? "null");
  } catch {
    saved = null;
  }
  const value = (typeof saved === "object" && saved !== null ? saved : {}) as Record<string, unknown>;
  return {
    role: ROLES.find((role) => role === value.role) ?? DEFAULT_SETTINGS.role,
    platformAdmin: value.platformAdmin === true,
    seed: PREVIEW_SEEDS.find((seed) => seed === value.seed) ?? DEFAULT_SETTINGS.seed,
    slowMs: SLOW_DELAYS_MS.find((delay) => delay === value.slowMs) ?? DEFAULT_SETTINGS.slowMs,
  };
}

export function saveSettings(storage: Storage | undefined, settings: PreviewSettings): void {
  try {
    storage?.setItem(SETTINGS_KEY, JSON.stringify(settings));
  } catch {
    // Storage refused (a private window): the page reloads with the defaults.
  }
}
