import type { MemberRole } from "../../api/pending/members.ts";
import type { MessageId } from "../../i18n/messages.ts";
import { cn } from "../../lib/utils.ts";

/** The roles, the one with the fewest rights first (the order of the invitation form's choices). */
export const ROLES: readonly MemberRole[] = ["viewer", "editor", "admin"];

export { ROLE_LABELS as ROLE_LABEL } from "../../session/tenant-name.ts";

/** What a role may do, in a line (design.md 「3.1」, the cards of the role choice). */
export const ROLE_SUMMARY: Readonly<Record<MemberRole, MessageId>> = {
  viewer: "members.role.viewer_summary",
  editor: "members.role.editor_summary",
  admin: "members.role.admin_summary",
};

/** A small label: brand-colored for what marks the user's own place (admin, "you"), plain otherwise. */
export function Badge({ strong = false, children }: { strong?: boolean; children: string }) {
  return (
    <span
      className={cn(
        "inline-flex items-center rounded-sm px-2 py-0.5 text-xs font-medium whitespace-nowrap",
        strong ? "bg-brand-soft text-primary" : "bg-secondary text-secondary-foreground",
      )}
    >
      {children}
    </span>
  );
}
