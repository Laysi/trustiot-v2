import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useCallback, useRef } from "react";
import { useLocation, useNavigate } from "react-router";

import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import type { Conversation } from "../../api/conversations.ts";
import { conversationPath } from "../../app/paths.ts";
import type { AskedState } from "../../chat/questions.ts";
import type { z } from "../../lib/zod.ts";
import { conversationListsKey } from "../../pages/conversations/queries.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { useConversationAsk } from "./conversation-ask.ts";
import { originState } from "./origin.ts";

/**
 * Starts a conversation, asks `question` in it and opens its page, which follows the answer, as
 * the home page's composer does. `back` names the page the button is on, so the conversation
 * leads back to it (`useAskOrigin`). A conversation that was started before the question failed
 * is used again by the next ask, so a retry leaves no empty conversation behind.
 */
function useAskInNewConversation() {
  const conversations = useConversationApi();
  const navigate = useNavigate();
  const location = useLocation();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const started = useRef<Conversation | null>(null);
  return useCallback(
    async (question: string, back: { label: string } | undefined) => {
      started.current ??= await conversations.createConversation();
      const conversation = started.current;
      const run = await conversations.askQuestion(conversation.id, question);
      // The lists (the home page's recent conversations) learn of the new conversation when they next show.
      void queryClient.invalidateQueries({ queryKey: conversationListsKey(tenant) });
      const asked: z.input<typeof AskedState> = { asked: { runId: run.id, startedAt: run.started_at, text: question } };
      const origin = back
        ? originState({ to: `${location.pathname}${location.search}${location.hash}`, label: back.label })
        : {};
      void navigate(conversationPath(conversation.id), { state: { ...asked, ...origin } });
    },
    [conversations, navigate, location, queryClient, tenant],
  );
}

/**
 * What a context button does with its question: inside a conversation (an answer's components) it
 * asks in that conversation, anywhere else it starts a new one and opens it. The state of the
 * send, and the error when it fails, are the button's own, so the message can stand under it.
 */
export function useAskQuestion(back: { label: string } | undefined) {
  const here = useConversationAsk();
  const startNew = useAskInNewConversation();
  const send = useMutation({
    mutationFn: (question: string) => (here !== null ? here.ask(question) : startNew(question, back)),
  });
  return {
    ask: send.mutate,
    pending: send.isPending,
    error: send.error,
    reset: send.reset,
    /** The conversation the button is in cannot take a question now. */
    blocked: here?.busy ?? false,
  };
}
