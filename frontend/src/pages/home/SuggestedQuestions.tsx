import { useQuery } from "@tanstack/react-query";
import {
  GitCompareArrowsIcon,
  type LucideIcon,
  SparklesIcon,
  TrendingUpIcon,
  TriangleAlertIcon,
  WifiOffIcon,
  ZapIcon,
} from "lucide-react";
import { useId } from "react";

import {
  MAX_SUGGESTIONS,
  type Suggestion,
  type SuggestionCategory,
  useSuggestionsApi,
} from "../../api/pending/suggestions.ts";
import { AskAiButton } from "../../components/ask/AskAiButton.tsx";
import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";

const ICONS: Readonly<Record<SuggestionCategory, LucideIcon>> = {
  energy: ZapIcon,
  offline: WifiOffIcon,
  anomaly: TriangleAlertIcon,
  compare: GitCompareArrowsIcon,
  trend: TrendingUpIcon,
};

/** The key of the tenant's suggestions: starts with the tenant, so another tenant's are never shown for this one. */
export const suggestionsKey = (tenant: string) => ["suggestions", tenant] as const;

/**
 * The suggested questions under the empty conversation's composer (docs/design/home-ai/design.md,
 * 「建議問題」, Laysi/trustiot-v2#158): pills that ask at once, filled by the server with this
 * tenant's devices (`api/pending/suggestions.ts`). A tenant that has no reading yet is told what
 * will appear instead. A failure takes one calm line with a retry, not an error: the composer
 * above works without them (design 12-C). Six pills on a wide screen, four on a phone, one a row.
 */
export function SuggestedQuestions() {
  const { t } = useI18n();
  const api = useSuggestionsApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const titleId = useId();
  const list = useQuery({
    queryKey: suggestionsKey(tenant),
    queryFn: ({ signal }) => api.listSuggestions(tenant, signal),
    enabled: tenant !== "",
  });

  if (list.isError) {
    return (
      <p role="status" className="flex flex-wrap items-center justify-center gap-x-1.5 text-sm text-muted-foreground">
        <SparklesIcon aria-hidden="true" className="size-4" />
        <span>{t("suggestions.failed")}</span>
        <span aria-hidden="true">・</span>
        <Button
          variant="link"
          size="sm"
          className="h-auto px-0"
          onClick={() => void list.refetch()}
          disabled={list.isFetching}
        >
          {t("page.action.retry")}
        </Button>
      </p>
    );
  }
  if (list.isPending) {
    return (
      <div aria-busy="true" className="grid gap-2">
        <p role="status" className="sr-only">
          {t("suggestions.loading")}
        </p>
        <ul aria-hidden="true" className="flex flex-wrap justify-center gap-2 max-sm:flex-col">
          {[0, 1, 2].map((slot) => (
            <li key={slot} className="h-9 w-56 animate-pulse rounded-full bg-muted max-sm:h-11 max-sm:w-full" />
          ))}
        </ul>
      </div>
    );
  }
  const suggestions = list.data.suggestions.slice(0, MAX_SUGGESTIONS);
  if (list.data.has_data && suggestions.length === 0) return null;
  return (
    <section aria-labelledby={titleId} className="grid gap-2">
      <h2
        id={titleId}
        className="flex items-center justify-center gap-1.5 text-[13px] font-medium text-muted-foreground max-sm:justify-start"
      >
        <SparklesIcon aria-hidden="true" className="size-3.5" />
        {t(list.data.has_data ? "suggestions.title" : "suggestions.empty")}
      </h2>
      {suggestions.length > 0 ? (
        <ul className="flex flex-wrap justify-center gap-2 max-sm:flex-col max-sm:items-stretch">
          {suggestions.map((suggestion, index) => (
            <SuggestionItem key={suggestion.id} suggestion={suggestion} index={index} />
          ))}
        </ul>
      ) : null}
    </section>
  );
}

function SuggestionItem({ suggestion, index }: { suggestion: Suggestion; index: number }) {
  return (
    // A phone has room for four.
    <li className={index >= 4 ? "max-sm:hidden" : undefined}>
      <AskAiButton
        question={suggestion.text}
        icon={ICONS[suggestion.category]}
        className="max-sm:w-full"
        buttonClassName="max-sm:w-full max-sm:rounded-xl"
      />
    </li>
  );
}
