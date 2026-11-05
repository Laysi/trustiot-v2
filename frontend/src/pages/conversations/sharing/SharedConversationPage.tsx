import { CopyPlusIcon, EyeIcon, MessagesSquareIcon } from "lucide-react";
import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import type { SharedConversationDetail } from "../../../api/conversations.ts";
import { classifyError } from "../../../api/errors.ts";
import { conversationPath, sharedConversationsPath } from "../../../app/paths.ts";
import { Thread } from "../../../chat/Thread.tsx";
import { Alert, AlertDescription } from "../../../components/ui/alert.tsx";
import { Button } from "../../../components/ui/button.tsx";
import { Spinner } from "../../../components/ui/spinner.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { Tag } from "../../dashboards/Tag.tsx";
import { StatusPage } from "../../StatusPage.tsx";
import { ConversationDashboards } from "../AnswerDashboards.tsx";
import { DraftPanel } from "../DraftPanel.tsx";
import { useConversationDrafts } from "../drafts.ts";
import { shownTitle } from "../queries.ts";
import { useCopyConversation, useSharedConversation } from "./queries.ts";

/** A conversation ID as the API names them; anything else is no shared conversation. */
const CONVERSATION_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * Another member's conversation, read only (`/shared-conversations/:conversationId`; the design
 * is in docs/design/chat-dashboards/design.md 「分享給整個租戶」): the title and facts, the
 * questions and answers as the owner's page draws them, one action, "Copy to my conversations",
 * and a note on what a copy holds. There is nothing of the owner's: no composer, stop,
 * regenerate, rename, archive, delete or share, and nothing to save. The conversation's dashboard
 * drafts, which the reader sees too, are in the draft panel in its read-only mode, beside the
 * conversation as on the owner's page. The owner can stop sharing at any time, then this page is
 * "not found".
 */
export function SharedConversationPage() {
  const { conversationId = "" } = useParams();
  if (!CONVERSATION_ID.test(conversationId)) {
    return <SharedNotFound />;
  }
  // Another conversation starts from a new page state: its own notices.
  return <SharedView key={conversationId.toLowerCase()} conversationId={conversationId.toLowerCase()} />;
}

function SharedView({ conversationId }: { conversationId: string }) {
  const { t, errorMessage } = useI18n();
  const conversation = useSharedConversation(conversationId);

  if (conversation.isPending) {
    return <SharedSkeleton label={t("share.view.loading")} />;
  }
  if (conversation.isError) {
    if (classifyError(conversation.error) === "not_found") {
      return <SharedNotFound />;
    }
    return (
      <Alert variant="destructive" className="mx-auto max-w-3xl grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("share.view.load_failed", { reason: errorMessage(conversation.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void conversation.refetch()} disabled={conversation.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }
  return <SharedContent conversation={conversation.data} />;
}

function SharedContent({ conversation }: { conversation: SharedConversationDetail }) {
  const i18n = useI18n();
  const { t, formatDateTime, errorMessage } = i18n;
  const navigate = useNavigate();
  const copy = useCopyConversation(conversation.id);
  const [failure, setFailure] = useState<unknown>(null);
  // The drafts of the owner's conversation, which the service lets a reader list and open (their
  // datasets run as the reader); a failure leaves the conversation as it is, with a message.
  const draftsQuery = useConversationDrafts(conversation.id);
  const drafts = draftsQuery.data ?? [];
  const title = shownTitle(conversation, i18n);

  const copyIt = () => {
    setFailure(null);
    copy.mutate(undefined, {
      onSuccess: (created) => navigate(conversationPath(created.id)),
      onError: (error) => setFailure(error),
    });
  };

  // The draft panel saves nothing here (`readOnly`), but its parts need what the conversation page
  // shares among its dashboards: the saved versions' memory and the limit on dataset runs.
  return (
    <ConversationDashboards>
      <div
        className={cn(
          "mx-auto grid w-full grid-cols-[minmax(0,1fr)] gap-6",
          drafts.length > 0 ? "xl:grid-cols-[minmax(0,1fr)_minmax(0,36rem)] xl:items-start" : "max-w-3xl",
        )}
      >
        <div className="mx-auto grid w-full min-w-0 max-w-3xl grid-cols-[minmax(0,1fr)] content-start gap-6">
          <header className="grid gap-3">
            <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
              <div className="grid min-w-0 flex-[1_1_16rem] gap-1">
                <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
                  <h1 className="min-w-0 text-xl font-semibold tracking-tight wrap-anywhere">{title}</h1>
                  <Tag icon={<EyeIcon aria-hidden="true" />}>{t("share.view.read_only")}</Tag>
                </div>
                <p className="text-sm text-muted-foreground">
                  {t("share.view.meta", {
                    shared: formatDateTime(conversation.shared_at),
                    activity: formatDateTime(conversation.last_activity_at),
                  })}
                </p>
              </div>
              <Button onClick={copyIt} disabled={copy.isPending}>
                {copy.isPending ? <Spinner /> : <CopyPlusIcon />}
                {copy.isPending ? t("share.copy.pending") : t("share.copy.button")}
              </Button>
            </div>
            <Alert role="note">
              <AlertDescription>
                <p className="text-foreground">{t("share.view.note")}</p>
              </AlertDescription>
            </Alert>
            {failure !== null ? (
              <Alert role="alert" variant="destructive">
                <AlertDescription>
                  <p>{t("share.copy.failed", { reason: errorMessage(failure) })}</p>
                </AlertDescription>
              </Alert>
            ) : null}
            {draftsQuery.isError ? (
              <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
                <AlertDescription className="col-start-1">
                  <p>{t("draft.list_failed", { reason: errorMessage(draftsQuery.error) })}</p>
                </AlertDescription>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => void draftsQuery.refetch()}
                  disabled={draftsQuery.isFetching}
                >
                  {t("page.action.retry")}
                </Button>
              </Alert>
            ) : null}
          </header>

          {conversation.messages.length === 0 ? (
            <p role="status" className="rounded-lg border px-4 py-8 text-center text-sm text-muted-foreground">
              {t("chat.empty")}
            </p>
          ) : (
            <Thread conversationId={conversation.id} messages={conversation.messages} linkBack={false} />
          )}
        </div>
        {drafts.length > 0 ? <DraftPanel conversationId={conversation.id} drafts={drafts} readOnly /> : null}
      </div>
    </ConversationDashboards>
  );
}

/** Stands in for the conversation while it loads; announced once. */
function SharedSkeleton({ label }: { label: string }) {
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
      </div>
    </section>
  );
}

/** A conversation that is not shared (any more), does not exist, was deleted or is the reader's own: one answer for all. */
function SharedNotFound() {
  const { t } = useI18n();
  return (
    <StatusPage
      title={t("share.not_found.title")}
      icon={<MessagesSquareIcon />}
      actions={
        <Button asChild variant="outline">
          <Link to={sharedConversationsPath()}>{t("share.not_found.back")}</Link>
        </Button>
      }
    >
      <p>{t("share.not_found.body")}</p>
    </StatusPage>
  );
}
