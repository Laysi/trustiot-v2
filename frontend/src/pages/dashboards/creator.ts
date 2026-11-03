import type { I18n } from "../../i18n/useI18n.ts";

/**
 * Who made a dashboard or a version: the API names a user by ID only (there is no way to look a
 * member up by it yet), so the page can say "you" or "another member" and no more.
 */
export function creatorLabel(userId: string, me: string | undefined, t: I18n["t"]): string {
  return userId === me ? t("dashboards.creator.you") : t("dashboards.creator.other");
}
