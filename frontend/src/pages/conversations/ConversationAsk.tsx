import { useQueryClient } from "@tanstack/react-query";
import { createContext, type ReactNode, useCallback, useContext, useMemo, useState } from "react";

import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import type { ConversationDetail } from "../../api/conversations.ts";
import { ConversationAskContext } from "../../components/ask/conversation-ask.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { conversationKey, conversationListsKey } from "./queries.ts";

/** A question a context button asked in this conversation: the run to follow and the text it asked. */
export interface AskedHere {
  runId: string;
  startedAt: string;
  text: string;
}

const AskedHereContext = createContext<AskedHere | null>(null);

/**
 * The latest question a context button of this page asked (`AskAiButton` in an answer's
 * components), for the page's turn to follow as it follows one typed in its composer
 * (ConversationTurn); `null` until one is asked.
 */
export function useAskedHere(): AskedHere | null {
  return useContext(AskedHereContext);
}

/**
 * Lets the context buttons inside a conversation ask in it (`ConversationAsk`): the question goes
 * to this conversation's question endpoint, the page learns of the run, and the conversation is
 * read again whatever the service answered, as ConversationTurn does for its own asks. The
 * service answers one question of a conversation at a time, so the buttons are off while one
 * runs or is on its way (`busy`).
 */
export function ConversationAskProvider({
  conversation,
  children,
}: {
  conversation: Pick<ConversationDetail, "id" | "latest_run">;
  children: ReactNode;
}) {
  const api = useConversationApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const [asked, setAsked] = useState<AskedHere | null>(null);
  const [sending, setSending] = useState(false);
  const { id } = conversation;
  const running = conversation.latest_run?.status === "running";

  const ask = useCallback(
    async (question: string) => {
      const refresh = () => queryClient.invalidateQueries({ queryKey: conversationKey(tenant, id) });
      setSending(true);
      let run: Awaited<ReturnType<typeof api.askQuestion>>;
      try {
        run = await api.askQuestion(id, question);
      } catch (error) {
        setSending(false);
        // The service knows the conversation's state better than the page does (a question may be running).
        void refresh();
        throw error;
      }
      setAsked({ runId: run.id, startedAt: run.started_at, text: question });
      // The conversation's activity changed, so it moves up in the lists.
      void queryClient.invalidateQueries({ queryKey: conversationListsKey(tenant) });
      try {
        // Off until the page has read the run, so no second button asks into a running question.
        await refresh();
      } finally {
        setSending(false);
      }
    },
    [api, id, queryClient, tenant],
  );

  const value = useMemo(() => ({ ask, busy: running || sending }), [ask, running, sending]);
  return (
    <ConversationAskContext.Provider value={value}>
      <AskedHereContext.Provider value={asked}>{children}</AskedHereContext.Provider>
    </ConversationAskContext.Provider>
  );
}
