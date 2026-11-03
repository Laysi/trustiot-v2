import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../session/PinSession.tsx";
import type { ConversationApi } from "./conversations.ts";

const ConversationApiContext = createContext<ConversationApi | null>(null);

/** Gives the pages the assistant's conversation endpoints: `conversationApi` in the app, a fake server in tests. */
export function ConversationApiProvider({ api, children }: { api: ConversationApi; children: ReactNode }) {
  return <ConversationApiContext.Provider value={api}>{children}</ConversationApiContext.Provider>;
}

/**
 * The conversation API, bound to the page: its requests name the page's user and tenant
 * (`PinSession`). Outside the pages (the top bar) it names the session the cache shows.
 */
export function useConversationApi(): ConversationApi {
  const value = useContext(ConversationApiContext);
  if (value === null) {
    throw new Error("useConversationApi needs a ConversationApiProvider");
  }
  return useSessionBound(value);
}
