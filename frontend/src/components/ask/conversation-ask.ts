import { createContext, useContext } from "react";

/**
 * Where a context button's question goes when the button is inside a conversation (an answer's
 * components): into that same conversation, not into a new one. The conversation page provides
 * it (pages/conversations/ConversationAsk.tsx); every other page leaves it out, and its buttons
 * start a new conversation (`useAskQuestion`).
 */
export interface ConversationAsk {
  /**
   * Asks `question` in this conversation. Resolves once the service accepted it and the page
   * knows about the run; rejects with the error the service answered with.
   */
  ask(question: string): Promise<void>;
  /** A question of this conversation is being answered or sent: the service takes one at a time. */
  busy: boolean;
}

export const ConversationAskContext = createContext<ConversationAsk | null>(null);

/** The conversation the button is in, or `null` outside one. */
export function useConversationAsk(): ConversationAsk | null {
  return useContext(ConversationAskContext);
}
