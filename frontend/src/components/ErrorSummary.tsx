import { useEffect, useRef } from "react";

import { useI18n } from "../i18n/useI18n.ts";
import { cn } from "../lib/utils.ts";
import { focusRing } from "./ui/button.tsx";

/** One line of the summary: what is wrong, and what pressing it does (focus the input). */
export interface SummaryItem {
  key: string;
  text: string;
  focus(): void;
}

/**
 * The list at the top of a form that a submit left with problems (docs/design/tenant-admin/,
 * 「錯誤處理」): it takes the focus when it appears or when `attempt` (the number of failed submits)
 * changes, and each line leads to its input.
 */
export function ErrorSummary({ items, attempt }: { items: readonly SummaryItem[]; attempt: number }) {
  const { t } = useI18n();
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (attempt > 0) ref.current?.focus();
  }, [attempt]);
  if (items.length === 0) return null;
  return (
    <div
      ref={ref}
      role="alert"
      tabIndex={-1}
      aria-labelledby="error-summary-title"
      className={cn("grid gap-2 rounded-lg border border-destructive/50 bg-card px-4 py-3 text-sm", focusRing)}
    >
      <p id="error-summary-title" className="font-medium text-destructive">
        {t("form.error_summary", { count: items.length })}
      </p>
      <ul className="grid gap-1">
        {items.map((item) => (
          <li key={item.key}>
            <button
              type="button"
              className={cn("rounded-sm text-left text-destructive underline underline-offset-4", focusRing)}
              onClick={item.focus}
            >
              {item.text}
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
