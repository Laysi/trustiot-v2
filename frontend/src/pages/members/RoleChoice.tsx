import { useId } from "react";

import type { MemberRole } from "../../api/pending/members.ts";
import { focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { ROLE_LABEL, ROLE_SUMMARY, ROLES } from "./roles.tsx";

/**
 * The three roles as radio cards, each with what it allows (the invitation and the role dialogs).
 * The radio is the real control, hidden but focusable; the card shows the choice and the focus.
 */
export function RoleChoice({
  value,
  onChange,
  disabled,
}: {
  value: MemberRole;
  onChange(role: MemberRole): void;
  disabled: boolean;
}) {
  const { t } = useI18n();
  const name = useId();
  return (
    <fieldset className="grid gap-2" disabled={disabled}>
      <legend className="mb-2 text-sm leading-none font-medium">{t("members.role.label")}</legend>
      {ROLES.map((role) => (
        <label
          key={role}
          className={cn(
            "flex cursor-pointer items-start gap-3 rounded-md border p-3 text-sm has-[:checked]:border-primary has-[:checked]:bg-accent has-[:disabled]:opacity-50",
            "has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-ring",
          )}
        >
          <input
            type="radio"
            name={name}
            value={role}
            checked={value === role}
            onChange={() => onChange(role)}
            className={cn("sr-only", focusRing)}
          />
          <span
            aria-hidden="true"
            className="mt-0.5 grid size-4 shrink-0 place-items-center rounded-full border border-input bg-background"
          >
            <span className={cn("size-2 rounded-full", value === role ? "bg-primary" : "bg-transparent")} />
          </span>
          <span className="grid gap-0.5">
            <span className="font-medium">{t(ROLE_LABEL[role])}</span>
            <span className="text-muted-foreground">{t(ROLE_SUMMARY[role])}</span>
          </span>
        </label>
      ))}
    </fieldset>
  );
}
