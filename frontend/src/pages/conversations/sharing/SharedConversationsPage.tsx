import { MessagesSquareIcon, SearchIcon, SearchXIcon, UsersIcon } from "lucide-react";
import { type ReactNode, useEffect, useId, useState } from "react";
import { Link } from "react-router";

import { sharedConversationPath } from "../../../app/paths.ts";
import { Alert, AlertDescription } from "../../../components/ui/alert.tsx";
import { Button, focusRing } from "../../../components/ui/button.tsx";
import { controlClasses } from "../../../components/ui/input.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { useSession } from "../../../session/SessionProvider.tsx";
import { PageHeader } from "../../dashboards/PageHeader.tsx";
import { HistoryTabs, useSetHistoryTab } from "../HistoryTabs.tsx";
import { shownTitle } from "../queries.ts";
import { useSharedPages } from "./queries.ts";

/**
 * The "Shared with me" tab of the conversation history (`/conversations?tab=shared`): what other
 * members of the tenant shared, the most recently active first, each a link to its read-only page
 * (docs/design/chat-dashboards/design.md 「分享給整個租戶」). The tab beside the user's own three
 * has the same head, tab buttons and search by title, and the table has no actions: a shared
 * conversation is only opened, and copied from its page.
 */
export function SharedConversationsPage() {
  const i18n = useI18n();
  const { t, errorMessage, formatDateTime } = i18n;
  const tenant = useSession().session?.activeTenant;
  const setTab = useSetHistoryTab();
  const [query, setQuery] = useState("");
  const searchId = useId();
  const list = useSharedPages();

  const search = query.trim().toLocaleLowerCase();
  // The API has no search: with a search term the rest of the pages is read, so the filter
  // covers every shared conversation, as in the user's own tabs.
  const { hasNextPage, isFetchingNextPage, isFetchNextPageError, fetchNextPage } = list;
  useEffect(() => {
    if (search !== "" && hasNextPage && !isFetchingNextPage && !isFetchNextPageError) void fetchNextPage();
  }, [search, hasNextPage, isFetchingNextPage, isFetchNextPageError, fetchNextPage]);

  const loaded = list.data?.pages.flatMap((page) => page.conversations) ?? [];
  const shown =
    search === "" ? loaded : loaded.filter((item) => shownTitle(item, i18n).toLocaleLowerCase().includes(search));
  const searchingMore = search !== "" && list.hasNextPage && !list.isFetchNextPageError;

  let body: ReactNode;
  if (list.isPending) {
    body = <ListSkeleton label={t("share.list.loading")} />;
  } else if (list.data === undefined) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("share.list.load_failed", { reason: errorMessage(list.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (shown.length === 0 && !searchingMore) {
    body = <EmptyList query={search === "" ? null : query.trim()} />;
  } else {
    body = (
      <section
        aria-label={t("share.list.label")}
        aria-busy={searchingMore}
        className="relative w-full overflow-x-auto rounded-lg border bg-card text-card-foreground"
      >
        <table className="w-full min-w-[40rem] text-sm">
          <caption className="sr-only">{t("share.list.caption", { count: shown.length })}</caption>
          <thead className="bg-muted/60 text-left text-muted-foreground">
            <tr>
              <th scope="col" className="px-4 py-2.5 font-medium">
                {t("conversations.column.title")}
              </th>
              <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
                {t("share.list.column.owner")}
              </th>
              <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
                {t("share.list.column.shared")}
              </th>
              <th scope="col" className="px-4 py-2.5 font-medium whitespace-nowrap">
                {t("conversations.column.activity")}
              </th>
            </tr>
          </thead>
          <tbody>
            {shown.map((item) => (
              <tr key={item.id} className="border-t">
                <th scope="row" className="min-w-64 px-4 py-3 text-left font-normal">
                  <Link
                    to={sharedConversationPath(item.id)}
                    className={cn("rounded-sm font-medium wrap-anywhere underline-offset-4 hover:underline", focusRing)}
                  >
                    {shownTitle(item, i18n)}
                  </Link>
                </th>
                {/* The API names the owner by user ID only, so the column says "another member". */}
                <td className="px-3 py-3 whitespace-nowrap">{t("share.owner_other")}</td>
                <td className="px-3 py-3 whitespace-nowrap">
                  <time dateTime={item.shared_at} className="tabular-nums">
                    {formatDateTime(item.shared_at)}
                  </time>
                </td>
                <td className="px-4 py-3 whitespace-nowrap">
                  <time dateTime={item.last_activity_at} className="tabular-nums">
                    {formatDateTime(item.last_activity_at)}
                  </time>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={t("conversations.title")} meta={t("share.list.description", { tenant: tenant?.name ?? "" })} />

      <div className="flex flex-wrap items-center justify-between gap-3">
        <HistoryTabs tabs={["active", "archived", "deleted", "shared"]} tab="shared" onChange={setTab} />
        <div className="relative w-full sm:w-72">
          <label htmlFor={searchId} className="sr-only">
            {t("conversations.search.label")}
          </label>
          <SearchIcon
            aria-hidden="true"
            className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
          />
          <input
            id={searchId}
            type="search"
            value={query}
            placeholder={t("conversations.search.placeholder")}
            className={cn(controlClasses, "pl-9")}
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
      </div>

      <Alert role="note" className="grid-cols-[1rem_1fr]">
        <UsersIcon aria-hidden="true" />
        <AlertDescription>
          <p className="text-foreground">{t("share.list.note")}</p>
        </AlertDescription>
      </Alert>

      {body}

      {list.data !== undefined ? (
        <p className="text-xs text-muted-foreground">
          {searchingMore
            ? t("conversations.searching_more")
            : t(list.hasNextPage ? "conversations.footer_more" : "share.list.footer", { count: shown.length })}
        </p>
      ) : null}
      {list.hasNextPage && search === "" && shown.length > 0 ? (
        <div>
          <Button variant="outline" onClick={() => void list.fetchNextPage()} disabled={list.isFetchingNextPage}>
            {list.isFetchingNextPage ? t("conversations.loading_more") : t("conversations.load_more")}
          </Button>
        </div>
      ) : null}
      {list.isFetchNextPageError ? (
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{t("share.list.load_failed", { reason: errorMessage(list.error) })}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void list.fetchNextPage()} disabled={list.isFetchingNextPage}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      ) : null}
    </div>
  );
}

/** Rows that stand in for the list while it loads; announced once, not row by row. */
function ListSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="grid gap-4 rounded-lg border bg-card px-4 py-5 sm:px-6">
      <p role="status" className="sr-only">
        {label}
      </p>
      {[0, 1, 2].map((row) => (
        <div key={row} aria-hidden="true" className="flex items-center justify-between gap-4">
          <span className="h-3 w-2/5 animate-pulse rounded bg-muted" />
          <span className="h-3 w-1/5 animate-pulse rounded bg-muted" />
          <span className="h-3 w-1/6 animate-pulse rounded bg-muted" />
        </div>
      ))}
    </section>
  );
}

function EmptyList({ query }: { query: string | null }) {
  const { t } = useI18n();
  return (
    <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
      {query !== null ? (
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
      ) : (
        <MessagesSquareIcon aria-hidden="true" className="size-6 text-muted-foreground" />
      )}
      <p className="font-semibold">
        {query !== null ? t("conversations.empty.search_title", { query }) : t("share.list.empty_title")}
      </p>
      <p className="text-sm text-muted-foreground">
        {t(query !== null ? "conversations.empty.search_body" : "share.list.empty_body")}
      </p>
    </div>
  );
}
