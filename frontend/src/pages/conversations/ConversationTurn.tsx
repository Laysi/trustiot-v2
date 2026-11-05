import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useLocation } from "react-router";

import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import type { ConversationDetail } from "../../api/conversations.ts";
import { Composer } from "../../chat/Composer.tsx";
import { AskedState, askFailureReason, runEndMessage } from "../../chat/questions.ts";
import { Question } from "../../chat/Thread.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { conversationKey, conversationListsKey } from "./queries.ts";
import { useQuestionFromAddress } from "./useQuestionFromAddress.ts";

/**
 * The question this page follows until it ends; `text` is unknown for one another tab started.
 * `startedAt` (the run's own) tells a newer question from a stale read of an older one.
 */
interface Following {
  runId: string;
  startedAt: string;
  text?: string;
}

/** How a followed question ended without an answer. */
interface Ended {
  kind: "cancelled" | "failed" | "interrupted";
  messageKey: string | null | undefined;
}

/**
 * The part of a conversation page that asks (Laysi/trustiot-v2#1044): below the thread, the
 * question being answered with a waiting line, and the composer fixed to the bottom of the
 * window (docs/design/chat-dashboards/conversation.html).
 *
 * A question is stored with its answer, so while it runs it is not among the messages: the page
 * draws it from the text it sent (or the one the home page passed in the router state), and
 * follows the run through the conversation's `latest_run`, which the page reads again every few
 * seconds while a question runs (ConversationPage). Stopping asks the service to stop and the
 * run stays `running`, with `stop_requested_at`, until it has stopped. When the run ends:
 *
 * - answered: the answer is the last message of the conversation the page just read;
 * - stopped: a note says so, and the question goes back into the composer;
 * - failed or interrupted: the message the service names for it, and the question goes back
 *   into the composer.
 *
 * A send that fails (no connection, no permission, a question already running, the service
 * refusing) leaves the text in the composer and says why.
 */
export function ConversationTurn({ conversation }: { conversation: ConversationDetail }) {
  const i18n = useI18n();
  const { t } = i18n;
  const api = useConversationApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const location = useLocation();
  const fromAddress = useQuestionFromAddress();
  const textarea = useRef<HTMLTextAreaElement>(null);
  const latest = conversation.latest_run;

  const [text, setText] = useState(fromAddress);
  const [following, setFollowing] = useState<Following | null>(() => {
    const asked = AskedState.safeParse(location.state);
    return asked.success ? asked.data.asked : null;
  });
  const [ended, setEnded] = useState<Ended | null>(null);

  const refresh = () => queryClient.invalidateQueries({ queryKey: conversationKey(tenant, conversation.id) });

  const ask = useMutation({
    mutationFn: (question: string) => api.askQuestion(conversation.id, question),
    onSuccess: (run, question) => {
      setFollowing({ runId: run.id, startedAt: run.started_at, text: question });
      setText("");
    },
    // Whatever the service refused, it knows the conversation's state better than the page does.
    onSettled: refresh,
  });
  const cancel = useMutation({
    mutationFn: (runId: string) => api.stopQuestion(conversation.id, runId),
    onSettled: refresh,
  });
  const resetCancel = cancel.reset;

  // A question the page did not send (another tab's, or still running when the page opened).
  useEffect(() => {
    if (following === null && latest?.status === "running")
      setFollowing({ runId: latest.id, startedAt: latest.started_at });
  }, [following, latest]);

  // Another tab started a newer question, so the followed one ended without this page seeing it
  // (an older run in a stale read has an older start, and is not this).
  const superseded =
    following !== null &&
    latest != null &&
    latest.id !== following.runId &&
    Date.parse(latest.started_at) > Date.parse(following.startedAt);
  const finished = following !== null && latest?.id === following.runId && latest.status !== "running";
  const over = finished || superseded;
  const answering = following !== null && !over;

  useEffect(() => {
    if (following === null || latest == null) return;
    if (superseded) {
      // What became of the older question is not known here; the newer one is followed next.
      setFollowing(null);
      resetCancel();
      return;
    }
    if (latest.id !== following.runId || latest.status === "running") return;
    setFollowing(null);
    resetCancel();
    // The conversation's title and place in the lists changed with the answer.
    void queryClient.invalidateQueries({ queryKey: conversationListsKey(tenant) });
    if (latest.status === "answered") return;
    setEnded({ kind: latest.status, messageKey: latest.message_key });
    // The question was not kept, so it goes back to be changed or sent again.
    const sent = following.text;
    if (sent !== undefined) setText((current) => (current === "" ? sent : current));
  }, [following, latest, superseded, queryClient, tenant, resetCancel]);

  // A prefilled question: the cursor at its end, where the user goes on typing.
  useEffect(() => {
    const box = textarea.current;
    if (fromAddress === "" || box === null) return;
    box.focus();
    box.setSelectionRange(fromAddress.length, fromAddress.length);
  }, [fromAddress]);

  const submit = () => {
    const question = text.trim();
    if (question === "" || answering) return;
    setEnded(null);
    ask.reset();
    ask.mutate(question);
  };

  const stopRequested = following !== null && latest?.id === following.runId && latest.stop_requested_at != null;
  const error = ask.error
    ? t("chat.composer.ask_failed", { reason: askFailureReason(i18n, ask.error) })
    : cancel.error
      ? t("chat.composer.stop_failed", { reason: i18n.errorMessage(cancel.error) })
      : ended?.kind === "failed" || ended?.kind === "interrupted"
        ? runEndMessage(i18n, ended.messageKey)
        : null;

  return (
    <>
      {conversation.messages.length === 0 && !answering && ended === null ? (
        <p role="status" className="rounded-lg border px-4 py-8 text-center text-sm text-muted-foreground">
          {t("chat.empty")}
        </p>
      ) : null}
      {answering ? <Waiting text={following.text} /> : null}
      <div className="sticky bottom-0 z-10 -mx-4 border-t bg-background px-4 pt-3 pb-4 sm:border-t-0">
        <Composer
          label={t("chat.composer.continue_label")}
          placeholder={t("chat.composer.continue_placeholder")}
          rows={1}
          value={text}
          onChange={setText}
          onSubmit={submit}
          sending={ask.isPending}
          answering={answering}
          stopping={answering && (cancel.isPending || cancel.isSuccess || stopRequested)}
          onStop={() => {
            if (following !== null) cancel.mutate(following.runId);
          }}
          error={error}
          notice={ended?.kind === "cancelled" ? t("chat.composer.stopped") : null}
          textareaRef={textarea}
        />
      </div>
    </>
  );
}

/** The question being answered (when its text is known) and the line that says the answer is on its way. */
function Waiting({ text }: { text: string | undefined }) {
  const { t } = useI18n();
  const list = useRef<HTMLOListElement>(null);
  useEffect(() => {
    list.current?.scrollIntoView?.({ block: "nearest" });
  }, []);
  return (
    <ol ref={list} aria-label={t("chat.thread.pending_label")} className="flex flex-col gap-6">
      {text !== undefined ? <Question text={text} /> : null}
      <li className="grid grid-cols-[2rem_minmax(0,1fr)] items-start gap-x-3">
        <img src="/favicon.svg" alt="" className="size-8" />
        <p role="status" className="inline-flex items-center gap-2 pt-1.5 text-sm text-muted-foreground">
          <Spinner />
          {t("chat.composer.working")}
        </p>
      </li>
    </ol>
  );
}
