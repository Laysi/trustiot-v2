import { ArrowRightIcon, MessageSquareIcon } from "lucide-react";
import { Link } from "react-router";

import { CONVERSATIONS_PATH, conversationPath } from "../../app/paths.ts";
import { focusRing } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { shownTitle, useRecentConversations } from "./queries.ts";

/**
 * The home page's 「最近的對話」: the five most recently active conversations, each a card that
 * opens it, and a link to the whole history. It sits where docs/design/chat-dashboards/home.html
 * has its suggestions, and is drawn like them. With no conversation the section is not there at
 * all; while the list loads nothing is drawn either, so the page does not jump when it is empty.
 * A failed read says so in one line, with a retry.
 */
export function RecentConversations() {
  const i18n = useI18n();
  const { t, formatDateTime, errorMessage } = i18n;
  const recent = useRecentConversations();

  if (recent.isError) {
    return (
      <p role="alert" className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted-foreground">
        {t("home.recent.failed", { reason: errorMessage(recent.error) })}
        <button
          type="button"
          onClick={() => void recent.refetch()}
          disabled={recent.isFetching}
          className={cn("rounded-sm font-medium text-primary underline underline-offset-4", focusRing)}
        >
          {t("page.action.retry")}
        </button>
      </p>
    );
  }
  const conversations = recent.data ?? [];
  if (conversations.length === 0) return null;

  return (
    <section aria-labelledby="home-recent" className="grid gap-3">
      <h2 id="home-recent" className="text-sm font-medium text-muted-foreground">
        {t("home.recent.title")}
      </h2>
      <ul className="grid grid-cols-[repeat(auto-fit,minmax(min(15rem,100%),1fr))] gap-2">
        {conversations.map((conversation) => (
          <li key={conversation.id} className="grid">
            <Link
              to={conversationPath(conversation.id)}
              className={cn(
                "grid grid-cols-[1rem_minmax(0,1fr)] content-start gap-x-2 gap-y-0.5 rounded-lg border bg-card p-3 text-card-foreground transition-colors hover:border-primary hover:bg-accent",
                focusRing,
              )}
            >
              <MessageSquareIcon aria-hidden="true" className="mt-0.5 size-4 text-muted-foreground" />
              <span className="line-clamp-2 font-medium wrap-anywhere">{shownTitle(conversation, i18n)}</span>
              <span className="col-start-2 text-xs text-muted-foreground">
                {t("home.recent.activity", { time: formatDateTime(conversation.last_activity_at) })}
              </span>
            </Link>
          </li>
        ))}
      </ul>
      <p className="text-sm">
        <Link
          to={CONVERSATIONS_PATH}
          className={cn("inline-flex items-center gap-1 rounded-sm font-medium text-primary", focusRing)}
        >
          {t("home.recent.all")}
          <ArrowRightIcon aria-hidden="true" className="size-4" />
        </Link>
      </p>
    </section>
  );
}
