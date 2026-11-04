import { useQuery } from "@tanstack/react-query";
import {
  ArchiveIcon,
  ArchiveRestoreIcon,
  DownloadIcon,
  EllipsisIcon,
  MessagesSquareIcon,
  PencilIcon,
  Trash2Icon,
  XIcon,
} from "lucide-react";
import { useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import type { ConversationDetail } from "../../api/conversations.ts";
import { classifyError } from "../../api/errors.ts";
import { CONVERSATIONS_PATH } from "../../app/paths.ts";
import { conversationMarkdown, downloadText, markdownFileName } from "../../chat/export.ts";
import { Thread } from "../../chat/Thread.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../../components/ui/dropdown-menu.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { Tag } from "../dashboards/Tag.tsx";
import { StatusPage } from "../StatusPage.tsx";
import { ConversationTurn } from "./ConversationTurn.tsx";
import { DeleteDialog } from "./DeleteDialog.tsx";
import { conversationKey, shownTitle, useConversationChange } from "./queries.ts";
import { RenameForm } from "./RenameForm.tsx";

/** How often a conversation is read again while a question of it is answered (no streaming yet: Laysi/trustiot-v2#147). */
const QUESTION_POLL_MS = 1500;

/** A conversation ID as the API names them; anything else is no conversation of the user's. */
const CONVERSATION_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * One conversation (`/conversations/:conversationId`): its questions and answers, oldest first,
 * in one column with no side panel; its title can be renamed in place, and its menu exports it as
 * Markdown, archives or unarchives it and deletes it. It is a conversation page: the top bar has
 * no way back to the conversation here (docs/design/app-shell.md). The layout follows
 * docs/design/chat-dashboards/conversation.html. The composer at the bottom asks in the
 * conversation, shows the question being answered and stops it (ConversationTurn); while a
 * question runs the page reads the conversation again every `QUESTION_POLL_MS`, for its `latest_run`.
 */
export function ConversationPage() {
  const { conversationId = "" } = useParams();
  if (!CONVERSATION_ID.test(conversationId)) {
    return <ConversationNotFound />;
  }
  // Another conversation starts from a new page state: its own notices and open forms.
  return <ConversationView key={conversationId.toLowerCase()} conversationId={conversationId.toLowerCase()} />;
}

function ConversationView({ conversationId }: { conversationId: string }) {
  const { t, errorMessage } = useI18n();
  const api = useConversationApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const conversation = useQuery({
    queryKey: conversationKey(tenant, conversationId),
    queryFn: ({ signal }) => api.getConversation(conversationId, signal),
    refetchInterval: (query) => (query.state.data?.latest_run?.status === "running" ? QUESTION_POLL_MS : false),
  });

  if (conversation.isPending) {
    return <ConversationSkeleton label={t("chat.loading")} />;
  }
  if (conversation.isError) {
    if (classifyError(conversation.error) === "not_found") {
      return <ConversationNotFound />;
    }
    return (
      <Alert variant="destructive" className="mx-auto max-w-3xl grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("chat.load_failed", { reason: errorMessage(conversation.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void conversation.refetch()} disabled={conversation.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }
  return <ConversationContent conversation={conversation.data} />;
}

function ConversationContent({ conversation }: { conversation: ConversationDetail }) {
  const i18n = useI18n();
  const { t, formatDateTime, errorMessage } = i18n;
  const navigate = useNavigate();
  const change = useConversationChange();
  const [renaming, setRenaming] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [notice, setNotice] = useState<{ kind: "ok" | "error"; text: string } | null>(null);
  const menuButton = useRef<HTMLButtonElement>(null);
  const renameButton = useRef<HTMLButtonElement>(null);
  const title = shownTitle(conversation, i18n);
  const archived = conversation.archived_at != null;

  const setArchived = (next: boolean) => {
    setNotice(null);
    change.mutate(
      { kind: "archive", conversationId: conversation.id, archived: next },
      {
        onSuccess: () =>
          setNotice({
            kind: "ok",
            text: t(next ? "chat.message.archived" : "chat.message.unarchived"),
          }),
        onError: (error) => setNotice({ kind: "error", text: errorMessage(error) }),
      },
    );
  };

  const exportMarkdown = () => {
    const content = conversationMarkdown(title, conversation.messages, {
      question: t("chat.export.question"),
      answer: t("chat.export.answer"),
    });
    downloadText(content, markdownFileName(title, t("chat.export.file_fallback")));
    setNotice({ kind: "ok", text: t("chat.message.exported") });
  };

  return (
    <div className="mx-auto grid w-full max-w-3xl grid-cols-[minmax(0,1fr)] gap-6">
      <header className="grid gap-3">
        {renaming ? (
          <RenameForm
            conversationId={conversation.id}
            current={conversation.title ?? ""}
            onDone={(renamed) => {
              setRenaming(false);
              if (renamed !== undefined) setNotice({ kind: "ok", text: t("chat.message.renamed") });
              // Back to the button that opened the form, once it is drawn again.
              requestAnimationFrame(() => renameButton.current?.focus());
            }}
          />
        ) : (
          <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
            <div className="grid min-w-0 flex-[1_1_16rem] gap-1">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
                <h1 className="min-w-0 text-xl font-semibold tracking-tight wrap-anywhere">{title}</h1>
                {archived ? <Tag icon={<ArchiveIcon aria-hidden="true" />}>{t("chat.archived")}</Tag> : null}
              </div>
              <p className="text-sm text-muted-foreground">
                {t("chat.meta", {
                  started: formatDateTime(conversation.created_at),
                  activity: formatDateTime(conversation.last_activity_at),
                })}
              </p>
            </div>
            <div className="flex items-center gap-1">
              <Button ref={renameButton} variant="outline" size="sm" onClick={() => setRenaming(true)}>
                <PencilIcon />
                {t("chat.rename")}
              </Button>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button ref={menuButton} variant="ghost" size="icon" aria-label={t("chat.actions")}>
                    <EllipsisIcon />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="w-56">
                  <DropdownMenuItem onSelect={exportMarkdown}>
                    <DownloadIcon />
                    {t("chat.export")}
                  </DropdownMenuItem>
                  <DropdownMenuItem disabled={change.isPending} onSelect={() => setArchived(!archived)}>
                    {archived ? <ArchiveRestoreIcon /> : <ArchiveIcon />}
                    {t(archived ? "chat.unarchive" : "chat.archive")}
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem variant="destructive" onSelect={() => setDeleting(true)}>
                    <Trash2Icon />
                    {t("chat.delete")}
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </div>
        )}
        {notice ? (
          <Alert
            role={notice.kind === "error" ? "alert" : "status"}
            variant={notice.kind === "error" ? "destructive" : "default"}
            className="grid-cols-[1fr_auto] items-center"
          >
            <AlertDescription className={cn("col-start-1", notice.kind === "ok" && "text-foreground")}>
              <p>{notice.text}</p>
            </AlertDescription>
            <Button variant="ghost" size="icon" aria-label={t("chat.dismiss")} onClick={() => setNotice(null)}>
              <XIcon />
            </Button>
          </Alert>
        ) : null}
      </header>

      <DeleteDialog
        conversationId={conversation.id}
        title={title}
        open={deleting}
        onOpenChange={setDeleting}
        running={conversation.latest_run?.status === "running"}
        returnFocus={menuButton}
        onDeleted={() => navigate(CONVERSATIONS_PATH, { state: { deleted: title } })}
      />

      {conversation.messages.length > 0 ? (
        <Thread conversationId={conversation.id} messages={conversation.messages} />
      ) : null}
      <ConversationTurn conversation={conversation} />
    </div>
  );
}

/** Stands in for the conversation while it loads; announced once. */
function ConversationSkeleton({ label }: { label: string }) {
  return (
    <section aria-busy="true" className="mx-auto grid w-full max-w-3xl gap-6">
      <p role="status" className="sr-only">
        {label}
      </p>
      <div aria-hidden="true" className="grid gap-2">
        <span className="h-6 w-2/5 animate-pulse rounded bg-muted" />
        <span className="h-3 w-1/3 animate-pulse rounded bg-muted" />
      </div>
      <span aria-hidden="true" className="h-10 w-1/2 animate-pulse self-end justify-self-end rounded-xl bg-muted" />
      <div aria-hidden="true" className="grid gap-2 pl-11">
        <span className="h-3 w-full animate-pulse rounded bg-muted" />
        <span className="h-3 w-4/5 animate-pulse rounded bg-muted" />
        <span className="h-3 w-3/5 animate-pulse rounded bg-muted" />
      </div>
    </section>
  );
}

/** A conversation that is not the user's, does not exist or was deleted: the same answer for all. */
function ConversationNotFound() {
  const { t } = useI18n();
  return (
    <StatusPage
      title={t("chat.not_found.title")}
      icon={<MessagesSquareIcon />}
      actions={
        <Button asChild variant="outline">
          <Link to={CONVERSATIONS_PATH}>{t("chat.back_to_list")}</Link>
        </Button>
      }
    >
      <p>{t("chat.not_found.body")}</p>
    </StatusPage>
  );
}
