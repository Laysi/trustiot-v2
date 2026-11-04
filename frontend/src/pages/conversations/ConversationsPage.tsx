import { useInfiniteQuery } from "@tanstack/react-query";
import {
  ArchiveIcon,
  ArchiveRestoreIcon,
  HistoryIcon,
  MessagesSquareIcon,
  PencilIcon,
  SearchIcon,
  SearchXIcon,
  Trash2Icon,
  UndoIcon,
} from "lucide-react";
import { type ReactNode, useEffect, useId, useState } from "react";
import { Link, useLocation, useNavigate, useSearchParams } from "react-router";

import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import { type Conversation, type DeletedConversation, MAX_PAGE } from "../../api/conversations.ts";
import { type ConversationsTab, conversationPath } from "../../app/paths.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { type I18n, useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { z } from "../../lib/zod.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { DeleteDialog } from "./DeleteDialog.tsx";
import { type HistoryTab, HistoryTabs, TAB_PARAM } from "./HistoryTabs.tsx";
import { conversationListKey, shownTitle, useConversationChange } from "./queries.ts";
import { RenameForm } from "./RenameForm.tsx";
import { SharedConversationsPage } from "./sharing/SharedConversationsPage.tsx";

/** `?tab=archived` and `?tab=deleted` are the other tabs, and `?tab=shared` the one of other members' (sharing/), so a link or a reload keeps the choice. */
const TABS: readonly ConversationsTab[] = ["active", "archived", "deleted"];

/** What a page that sent the user here wants said (the conversation page after deleting one). */
const NoticeState = z.object({ deleted: z.string() });

type Message = { kind: "ok" | "error"; text: string; showDeleted?: boolean };
type Row = { kind: "live"; conversation: Conversation } | { kind: "deleted"; conversation: DeletedConversation };

function tabFrom(params: URLSearchParams): ConversationsTab {
  const value = params.get(TAB_PARAM);
  return value === "archived" || value === "deleted" ? value : "active";
}

/** One tab's conversations, page by page (100 a page, the service's most), the newest first. */
function useConversationPages(tab: ConversationsTab) {
  const api = useConversationApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useInfiniteQuery({
    queryKey: conversationListKey(tenant, tab),
    queryFn: async ({ pageParam, signal }): Promise<{ rows: Row[]; next: string | null }> => {
      const page = { limit: MAX_PAGE, ...(pageParam !== undefined ? { cursor: pageParam } : {}) };
      if (tab === "deleted") {
        const list = await api.listDeleted(page, signal);
        return {
          rows: list.conversations.map((conversation) => ({ kind: "deleted", conversation })),
          next: list.next_cursor ?? null,
        };
      }
      const list = await api.listConversations({ archived: tab === "archived", ...page }, signal);
      return {
        rows: list.conversations.map((conversation) => ({ kind: "live", conversation })),
        next: list.next_cursor ?? null,
      };
    },
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next ?? undefined,
  });
}

function rowTitle(row: Row, i18n: I18n): string {
  return shownTitle(row.conversation, i18n);
}

/**
 * The user's conversation history (`/conversations`): "In use", "Archived" and "Deleted" tabs,
 * a search by title, and for each conversation its page, renaming, archiving or unarchiving and
 * deleting; a deleted one can be restored for 30 days. The layout follows
 * docs/design/chat-dashboards/list.html; the page itself is designed in that folder's design.md
 * (「對話紀錄」). Conversations are the user's own: nobody else in the tenant sees them.
 */
export function ConversationsPage() {
  const [params] = useSearchParams();
  if (params.get(TAB_PARAM) === "shared") return <SharedConversationsPage />;
  return <ConversationHistory />;
}

function ConversationHistory() {
  const i18n = useI18n();
  const { t, errorMessage } = i18n;
  const { session } = useSession();
  const tenant = session?.activeTenant;
  const location = useLocation();
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const tab = tabFrom(params);
  const [query, setQuery] = useState("");
  const [renaming, setRenaming] = useState<string | null>(null);
  const [message, setMessage] = useState<Message | null>(() => {
    const notice = NoticeState.safeParse(location.state);
    return notice.success
      ? { kind: "ok", text: t("conversations.message.deleted", { title: notice.data.deleted }), showDeleted: true }
      : null;
  });
  const searchId = useId();
  const change = useConversationChange();
  const list = useConversationPages(tab);

  // The notice came with the navigation; it is not shown again on a reload or going back. Other
  // state (where the page was opened from, for the way back) stays.
  useEffect(() => {
    if (NoticeState.safeParse(location.state).success) {
      navigate(`${location.pathname}${location.search}`, { replace: true, state: null });
    }
  }, [location.state, location.pathname, location.search, navigate]);

  const search = query.trim().toLocaleLowerCase();
  // The API has no search: with a search term, the rest of the tab's pages are read, so the
  // filter covers every conversation, not only those already shown.
  const { hasNextPage, isFetchingNextPage, isFetchNextPageError, fetchNextPage } = list;
  useEffect(() => {
    if (search !== "" && hasNextPage && !isFetchingNextPage && !isFetchNextPageError) void fetchNextPage();
  }, [search, hasNextPage, isFetchingNextPage, isFetchNextPageError, fetchNextPage]);

  const loaded = list.data?.pages.flatMap((page) => page.rows) ?? [];
  const shown =
    search === "" ? loaded : loaded.filter((row) => rowTitle(row, i18n).toLocaleLowerCase().includes(search));
  const searchingMore = search !== "" && list.hasNextPage && !list.isFetchNextPageError;

  const setTab = (next: HistoryTab) => {
    setMessage(null);
    setRenaming(null);
    setParams(
      (current) => {
        const params = new URLSearchParams(current);
        if (next === "active") params.delete(TAB_PARAM);
        else params.set(TAB_PARAM, next);
        return params;
      },
      // A tab is the same page: where it was opened from (the way back) stays.
      { replace: true, state: location.state },
    );
  };

  const run = (
    request: Parameters<typeof change.mutate>[0],
    title: string,
    done: "archived" | "unarchived" | "restored",
  ) => {
    setMessage(null);
    change.mutate(request, {
      onSuccess: () => setMessage({ kind: "ok", text: t(`conversations.message.${done}`, { title }) }),
      onError: (error) => setMessage({ kind: "error", text: errorMessage(error) }),
    });
  };

  let body: ReactNode;
  if (list.isPending) {
    body = <ListSkeleton label={t("conversations.loading")} />;
  } else if (list.data === undefined) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("conversations.load_failed", { reason: errorMessage(list.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void list.refetch()} disabled={list.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (shown.length === 0 && !searchingMore) {
    body = <EmptyList tab={tab} query={search === "" ? null : query.trim()} />;
  } else {
    body = (
      <section
        aria-label={t("conversations.list_label")}
        aria-busy={searchingMore}
        className="relative w-full overflow-x-auto rounded-lg border bg-card text-card-foreground"
      >
        <table className="w-full min-w-[40rem] text-sm">
          <caption className="sr-only">{t(`conversations.caption.${tab}`, { count: shown.length })}</caption>
          <thead className="bg-muted/60 text-left text-muted-foreground">
            <tr>
              <th scope="col" className="px-4 py-2.5 font-medium">
                {t("conversations.column.title")}
              </th>
              {tab === "deleted" ? (
                <>
                  <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
                    {t("conversations.column.deleted")}
                  </th>
                  <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
                    {t("conversations.column.restorable_until")}
                  </th>
                </>
              ) : (
                <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
                  {t("conversations.column.activity")}
                </th>
              )}
              <th scope="col" className="px-4 py-2.5">
                <span className="sr-only">{t("conversations.column.actions")}</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {shown.map((row) => (
              <ConversationRow
                key={row.conversation.id}
                row={row}
                renaming={renaming === row.conversation.id}
                pending={change.isPending && change.variables?.conversationId === row.conversation.id}
                onRename={() => {
                  setMessage(null);
                  setRenaming(row.conversation.id);
                }}
                onRenamed={(renamed) => {
                  setRenaming(null);
                  if (renamed !== undefined) {
                    setMessage({ kind: "ok", text: t("conversations.message.renamed", { title: renamed }) });
                  }
                }}
                onArchive={(archived) =>
                  run(
                    { kind: "archive", conversationId: row.conversation.id, archived },
                    rowTitle(row, i18n),
                    archived ? "archived" : "unarchived",
                  )
                }
                onDeleted={() =>
                  setMessage({
                    kind: "ok",
                    text: t("conversations.message.deleted", { title: rowTitle(row, i18n) }),
                    showDeleted: true,
                  })
                }
                onRestore={() =>
                  run({ kind: "restore", conversationId: row.conversation.id }, rowTitle(row, i18n), "restored")
                }
              />
            ))}
          </tbody>
        </table>
      </section>
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={t("conversations.title")}
        meta={t("conversations.description", { tenant: tenant?.name ?? "" })}
      />

      <div className="flex flex-wrap items-center justify-between gap-3">
        <HistoryTabs tabs={[...TABS, "shared"]} tab={tab} onChange={setTab} />
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

      {tab === "deleted" ? (
        <Alert role="note" className="grid-cols-[1rem_1fr]">
          <HistoryIcon aria-hidden="true" />
          <AlertDescription>
            <p className="text-foreground">{t("conversations.deleted_note")}</p>
          </AlertDescription>
        </Alert>
      ) : null}

      {body}

      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2 text-xs text-muted-foreground">
        {/* Writes say what they did here, beside the list, and are read out. */}
        <p
          role="status"
          className={cn(
            "flex min-h-4 flex-wrap items-center gap-x-2",
            message?.kind === "error" ? "text-destructive" : "text-foreground",
          )}
        >
          {message?.text}
          {message?.showDeleted && tab !== "deleted" ? (
            <Button variant="link" size="sm" className="h-auto text-xs" onClick={() => setTab("deleted")}>
              {t("conversations.message.show_deleted")}
            </Button>
          ) : null}
        </p>
        {list.data !== undefined ? (
          <p>
            {searchingMore
              ? t("conversations.searching_more")
              : t(list.hasNextPage ? "conversations.footer_more" : `conversations.footer.${tab}`, {
                  count: shown.length,
                })}
          </p>
        ) : null}
      </div>
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
            <p>{t("conversations.load_failed", { reason: errorMessage(list.error) })}</p>
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

function EmptyList({ tab, query }: { tab: ConversationsTab; query: string | null }) {
  const { t } = useI18n();
  return (
    <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
      {query !== null ? (
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
      ) : (
        <MessagesSquareIcon aria-hidden="true" className="size-6 text-muted-foreground" />
      )}
      <p className="font-semibold">
        {query !== null ? t("conversations.empty.search_title", { query }) : t(`conversations.empty.${tab}_title`)}
      </p>
      <p className="text-sm text-muted-foreground">
        {t(query !== null ? "conversations.empty.search_body" : `conversations.empty.${tab}_body`)}
      </p>
    </div>
  );
}

/** One conversation of the list: its title (a link to it), its times and its actions. */
function ConversationRow(props: {
  row: Row;
  renaming: boolean;
  pending: boolean;
  onRename(): void;
  onRenamed(renamed?: string): void;
  onArchive(archived: boolean): void;
  onDeleted(): void;
  onRestore(): void;
}) {
  const i18n = useI18n();
  const { t, formatDateTime } = i18n;
  const { row } = props;
  const title = rowTitle(row, i18n);
  const [deleting, setDeleting] = useState(false);

  if (row.kind === "deleted") {
    const { conversation } = row;
    return (
      <tr className="border-t">
        <th scope="row" className="min-w-64 px-4 py-3 text-left font-medium wrap-anywhere">
          {title}
        </th>
        <td className="px-3 py-3 whitespace-nowrap">
          <time dateTime={conversation.deleted_at} className="tabular-nums">
            {formatDateTime(conversation.deleted_at)}
          </time>
        </td>
        <td className="px-3 py-3 whitespace-nowrap">
          <time dateTime={conversation.restorable_until} className="tabular-nums">
            {formatDateTime(conversation.restorable_until)}
          </time>
        </td>
        <td className="px-4 py-3">
          <div className="flex justify-end">
            <Button
              variant="outline"
              size="sm"
              disabled={props.pending}
              aria-label={t("conversations.restore_named", { title })}
              onClick={props.onRestore}
            >
              <UndoIcon />
              {t("conversations.restore")}
            </Button>
          </div>
        </td>
      </tr>
    );
  }

  const { conversation } = row;
  const archived = conversation.archived_at != null;
  return (
    <tr className="border-t">
      <th scope="row" className="min-w-64 px-4 py-3 text-left font-normal">
        {props.renaming ? (
          <RenameForm conversationId={conversation.id} current={conversation.title ?? ""} onDone={props.onRenamed} />
        ) : (
          <span className="grid justify-items-start gap-1">
            <Link
              to={conversationPath(conversation.id)}
              className={cn("rounded-sm font-medium wrap-anywhere underline-offset-4 hover:underline", focusRing)}
            >
              {title}
            </Link>
            {archived && conversation.archived_at ? (
              <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
                <ArchiveIcon aria-hidden="true" className="size-3.5" />
                {t("conversations.archived_at", { time: formatDateTime(conversation.archived_at) })}
              </span>
            ) : null}
          </span>
        )}
      </th>
      <td className="px-3 py-3 whitespace-nowrap">
        <time dateTime={conversation.last_activity_at} className="tabular-nums">
          {formatDateTime(conversation.last_activity_at)}
        </time>
      </td>
      <td className="px-4 py-3">
        <div className="flex flex-wrap items-center justify-end gap-1">
          <Button
            variant="ghost"
            size="sm"
            disabled={props.pending || props.renaming}
            aria-label={t("conversations.rename_named", { title })}
            onClick={props.onRename}
          >
            <PencilIcon />
            {t("conversations.rename")}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            disabled={props.pending}
            aria-label={t(archived ? "conversations.unarchive_named" : "conversations.archive_named", { title })}
            onClick={() => props.onArchive(!archived)}
          >
            {archived ? <ArchiveRestoreIcon /> : <ArchiveIcon />}
            {t(archived ? "conversations.unarchive" : "conversations.archive")}
          </Button>
          <DeleteDialog
            conversationId={conversation.id}
            title={title}
            open={deleting}
            onOpenChange={setDeleting}
            onDeleted={props.onDeleted}
            trigger={
              <Button
                variant="ghost"
                size="sm"
                disabled={props.pending}
                className="text-destructive hover:text-destructive"
                aria-label={t("conversations.delete_named", { title })}
              >
                <Trash2Icon />
                {t("conversations.delete")}
              </Button>
            }
          />
        </div>
      </td>
    </tr>
  );
}
