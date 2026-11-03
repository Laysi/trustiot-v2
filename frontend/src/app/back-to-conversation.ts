import { useLocation } from "react-router";

import { z } from "../lib/zod.ts";
import { useRecentConversations } from "../pages/conversations/queries.ts";
import { conversationPath } from "./paths.ts";

/** What a link inside a conversation (an answer's link) leaves in the router's state. */
const FromConversation = z.object({ fromConversation: z.guid() });

/**
 * The state a link from inside conversation `conversationId` passes along, so that the page it
 * opens leads back to that conversation (`useBackToConversation`).
 */
export function fromConversationState(conversationId: string): z.input<typeof FromConversation> {
  return { fromConversation: conversationId };
}

/**
 * Where the top bar's 「回到對話」 leads on a secondary page (docs/design/app-shell.md, 「原則」):
 * to the conversation the page was opened from, when a link inside it opened the page; else (a
 * bookmark, a typed address, a page changed from the menu) to the most recently active
 * conversation; and to the conversation home when there is none, or while that is not known yet.
 * Pages that are conversations (`secondary` false) read nothing.
 */
export function useBackToConversation(secondary: boolean): string {
  const location = useLocation();
  const from = FromConversation.safeParse(location.state);
  const recent = useRecentConversations(secondary && !from.success);
  if (from.success) return conversationPath(from.data.fromConversation);
  const latest = recent.data?.[0];
  return latest ? conversationPath(latest.id) : "/";
}
