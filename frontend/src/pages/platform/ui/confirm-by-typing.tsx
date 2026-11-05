import { type ReactNode, useId } from "react";

import { Input } from "../../../components/ui/input.tsx";
import { Label } from "../../../components/ui/label.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";

/** Whether `typed` is the string to confirm with; surrounding spaces are not held against the person. */
export function typedMatches(typed: string, expected: string): boolean {
  return typed.trim() === expected;
}

/**
 * The field of a high-risk confirmation (design.md, 「高風險操作的確認分級」): the person types
 * the tenant's name or the hardware ID, and the dialog's button stays disabled until it matches.
 * The label spells out the string to type; a mismatch is explained under the field, not only by
 * the disabled button.
 */
export function ConfirmByTyping({
  label,
  expected,
  value,
  onChange,
  disabled,
}: {
  /** The label with the string in it, e.g. "Type the tenant name “Demo” to confirm". */
  label: ReactNode;
  expected: string;
  value: string;
  onChange(value: string): void;
  disabled?: boolean;
}) {
  const { t } = useI18n();
  const id = useId();
  const errorId = `${id}-error`;
  const mismatch = value !== "" && !typedMatches(value, expected);
  return (
    <div className="grid gap-2">
      <Label htmlFor={id}>{label}</Label>
      <Input
        id={id}
        value={value}
        autoComplete="off"
        spellCheck={false}
        disabled={disabled ?? false}
        aria-invalid={mismatch ? true : undefined}
        aria-describedby={mismatch ? errorId : undefined}
        onChange={(event) => onChange(event.target.value)}
      />
      {mismatch ? (
        <p id={errorId} className="text-sm font-medium text-destructive">
          {t("platform.confirm.mismatch")}
        </p>
      ) : null}
    </div>
  );
}
