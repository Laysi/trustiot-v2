import { useMutation, useQueryClient } from "@tanstack/react-query";
import { SparklesIcon } from "lucide-react";
import { useRef } from "react";
import { useNavigate } from "react-router";
import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import type { Conversation } from "../../api/conversations.ts";
import { conversationPath } from "../../app/paths.ts";
import { type AskedState, askFailureReason } from "../../chat/questions.ts";
import { focusRing } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import type { z } from "../../lib/zod.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { conversationListsKey } from "../conversations/queries.ts";

/**
 * The "ask AI" capsule of docs/design/ms4-ms5/design.md 「問 AI 入口」: a fixed question, ending in
 * a question mark, that starts a conversation and sends it at once (decision Q5), then opens that
 * conversation's page, which follows the answer. `about` names what the question is about (the
 * report and its period); it is added to the question's text, because the conversation has no
 * context chips yet and the assistant needs to know which report is meant. The shared button of
 * Laysi/trustiot-v2#158 replaces this one.
 */
export function AskAiButton({ question, about }: { question: string; about?: string }) {
  const i18n = useI18n();
  const conversations = useConversationApi();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  // A conversation started before the question failed is used again by the next click.
  const started = useRef<Conversation | null>(null);
  const text = about ? `${question} (${about})` : question;

  const ask = useMutation({
    mutationFn: async () => {
      started.current ??= await conversations.createConversation();
      const conversation = started.current;
      const run = await conversations.askQuestion(conversation.id, text);
      return { conversation, run };
    },
    onSuccess: async ({ conversation, run }) => {
      await queryClient.invalidateQueries({ queryKey: conversationListsKey(tenant) });
      const state: z.input<typeof AskedState> = { asked: { runId: run.id, startedAt: run.started_at, text } };
      void navigate(conversationPath(conversation.id), { state });
    },
  });

  return (
    <span className="inline-grid gap-1">
      <button
        type="button"
        disabled={ask.isPending || ask.isSuccess}
        className={cn(
          "inline-flex h-8 items-center gap-2 rounded-full border bg-card px-3 text-sm font-medium transition-colors hover:bg-brand-soft",
          focusRing,
        )}
        onClick={() => {
          ask.reset();
          ask.mutate();
        }}
      >
        {ask.isPending ? <Spinner /> : <SparklesIcon aria-hidden="true" className="size-4 text-primary" />}
        {question}
      </button>
      {ask.isError ? (
        <span role="alert" className="text-xs text-destructive">
          {i18n.t("chat.composer.ask_failed", { reason: askFailureReason(i18n, ask.error) })}
        </span>
      ) : null}
    </span>
  );
}
