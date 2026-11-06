import { useLocation, useSearchParams } from "react-router";

import type { ConversationsTab } from "../../app/paths.ts";
import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";

/** The conversation history's tabs: the three of the user's own, and "Shared with me" where sharing exists. */
export type HistoryTab = ConversationsTab | "shared";

/** `?tab=...` holds the tab, so a link or a reload keeps the choice. */
export const TAB_PARAM = "tab";

/** The segmented buttons that choose the history's tab (docs/design/chat-dashboards/design.md 「對話紀錄」). */
export function HistoryTabs({
  tabs,
  tab,
  onChange,
}: {
  tabs: readonly HistoryTab[];
  tab: HistoryTab;
  onChange(next: HistoryTab): void;
}) {
  const { t } = useI18n();
  return (
    <fieldset className="inline-flex min-w-0 gap-0.5 rounded-md bg-muted p-[3px]">
      <legend className="sr-only">{t("conversations.tabs_label")}</legend>
      {tabs.map((value) => (
        <Button
          key={value}
          size="sm"
          variant="ghost"
          aria-pressed={tab === value}
          className={cn(
            "h-[30px] px-3 text-muted-foreground hover:bg-transparent",
            tab === value && "bg-background text-foreground shadow-sm hover:bg-background",
          )}
          onClick={() => onChange(value)}
        >
          {value === "shared" ? t("share.tab") : t(`conversations.tab.${value}`)}
        </Button>
      ))}
    </fieldset>
  );
}

/** Changes the tab in the address: the same page, so where it was opened from (the way back) stays. */
export function useSetHistoryTab(): (next: HistoryTab) => void {
  const location = useLocation();
  const [, setParams] = useSearchParams();
  return (next) =>
    setParams(
      (current) => {
        const params = new URLSearchParams(current);
        if (next === "active") params.delete(TAB_PARAM);
        else params.set(TAB_PARAM, next);
        return params;
      },
      { replace: true, state: location.state },
    );
}
