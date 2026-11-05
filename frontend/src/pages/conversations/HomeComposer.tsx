import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router";

import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import type { Conversation } from "../../api/conversations.ts";
import { conversationPath } from "../../app/paths.ts";
import { Composer } from "../../chat/Composer.tsx";
import { type AskedState, askFailureReason } from "../../chat/questions.ts";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import type { z } from "../../lib/zod.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { conversationListsKey } from "./queries.ts";
import { useQuestionFromAddress } from "./useQuestionFromAddress.ts";

/**
 * The home page's composer, the full-width box a user opens TrustIoT to (home.html): sending
 * starts a conversation and asks the question in it, then opens that conversation's page, which
 * shows the question and follows the answer. A question named in the address (`?q=`) is already
 * in the box. When the question cannot be sent the text stays in the box and the message says
 * why; a conversation that was started before the question failed is used again by the next
 * send, so a retry does not leave empty conversations behind.
 */
export function HomeComposer() {
  const i18n = useI18n();
  const { t } = i18n;
  const conversations = useConversationApi();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const fromAddress = useQuestionFromAddress();
  const textarea = useRef<HTMLTextAreaElement>(null);
  const [text, setText] = useState(fromAddress);
  const started = useRef<Conversation | null>(null);

  useEffect(() => {
    const box = textarea.current;
    if (fromAddress === "" || box === null) return;
    box.focus();
    box.setSelectionRange(fromAddress.length, fromAddress.length);
  }, [fromAddress]);

  const send = useMutation({
    mutationFn: async (question: string) => {
      started.current ??= await conversations.createConversation();
      const conversation = started.current;
      const run = await conversations.askQuestion(conversation.id, question);
      return { conversation, runId: run.id, startedAt: run.started_at, question };
    },
    onSuccess: async ({ conversation, runId, startedAt, question }) => {
      await queryClient.invalidateQueries({ queryKey: conversationListsKey(tenant) });
      const state: z.input<typeof AskedState> = { asked: { runId, startedAt, text: question } };
      void navigate(conversationPath(conversation.id), { state });
    },
  });

  const submit = () => {
    const question = text.trim();
    if (question === "" || send.isPending) return;
    send.reset();
    send.mutate(question);
  };

  const reason = send.error ? askFailureReason(i18n, send.error) : null;
  return (
    <Composer
      label={t("chat.composer.ask_label")}
      placeholder={t("chat.composer.ask_placeholder")}
      rows={3}
      value={text}
      onChange={setText}
      onSubmit={submit}
      sending={send.isPending || send.isSuccess}
      answering={false}
      stopping={false}
      onStop={() => {}}
      status={
        send.isPending || send.isSuccess ? (
          <>
            <Spinner />
            {t("chat.composer.working")}
          </>
        ) : null
      }
      error={reason ? t("chat.composer.ask_failed", { reason }) : null}
      textareaRef={textarea}
    />
  );
}
