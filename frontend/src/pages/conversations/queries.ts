import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { useConversationApi } from "../../api/ConversationApiProvider.tsx";
import type { Conversation, DeletedConversation } from "../../api/conversations.ts";
import type { ConversationsTab } from "../../app/paths.ts";
import type { I18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";

// The conversation pages' query keys. Each starts with the active tenant's ID, so a cached answer
// is never shown for another tenant (a tenant switch resets every query that is not the user's
// anyway, session/SessionProvider.tsx). Every write reads the lists and the conversation again,
// whether or not it succeeded, so what is shown is what the server holds.

/** Every list of the user's conversations: the history's tabs, the home page's and the top bar's latest. */
export const conversationListsKey = (tenant: string) => ["conversations", tenant, "list"] as const;

/** One tab of the history. */
export const conversationListKey = (tenant: string, tab: ConversationsTab) =>
  [...conversationListsKey(tenant), tab] as const;

/** The most recently active conversations: the home page's list, and the top bar's way back. */
export const recentConversationsKey = (tenant: string) => [...conversationListsKey(tenant), "recent"] as const;

/** How many conversations the home page lists (Laysi/trustiot-v2#1042). */
export const RECENT_COUNT = 5;

/**
 * The user's most recently active conversations in the active tenant, newest first: at most
 * `RECENT_COUNT`, archived ones left out. The home page lists them and the top bar's way back
 * takes the first, so both read one request. `enabled` is false where neither needs it.
 */
export function useRecentConversations(enabled = true) {
  const api = useConversationApi();
  const tenant = useSession().session?.activeTenant?.id;
  return useQuery({
    queryKey: recentConversationsKey(tenant ?? ""),
    queryFn: async ({ signal }) =>
      (await api.listConversations({ archived: false, limit: RECENT_COUNT }, signal)).conversations,
    enabled: enabled && tenant !== undefined,
  });
}

export const conversationKey = (tenant: string, conversationId: string) =>
  ["conversations", tenant, "detail", conversationId] as const;

/**
 * The title a conversation is shown with: its own, or until it has one (a title from the model
 * comes with Laysi/trustiot-v2#651) a provisional one from when it was started.
 */
export function shownTitle(
  conversation:
    | Pick<Conversation, "title" | "created_at">
    | (Pick<DeletedConversation, "title"> & { created_at?: string }),
  i18n: Pick<I18n, "t" | "formatDateTime">,
): string {
  const title = conversation.title?.trim();
  if (title) return title;
  return conversation.created_at
    ? i18n.t("conversations.untitled_started", { time: i18n.formatDateTime(conversation.created_at) })
    : i18n.t("conversations.untitled");
}

type Change =
  | { kind: "rename"; conversationId: string; title: string }
  | { kind: "archive"; conversationId: string; archived: boolean }
  | { kind: "delete"; conversationId: string }
  | { kind: "restore"; conversationId: string };

/**
 * Renames, archives or unarchives, deletes or restores a conversation; each takes effect at once
 * (deleting is confirmed first by the page). The lists and the conversation are read again
 * afterwards, whether or not it succeeded.
 */
export function useConversationChange() {
  const api = useConversationApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useMutation({
    mutationFn: async (change: Change): Promise<void> => {
      switch (change.kind) {
        case "rename":
          await api.updateConversation(change.conversationId, { title: change.title });
          return;
        case "archive":
          await api.updateConversation(change.conversationId, { archived: change.archived });
          return;
        case "delete":
          await api.deleteConversation(change.conversationId);
          return;
        case "restore":
          await api.restoreConversation(change.conversationId);
          return;
      }
    },
    onSettled: async (_result, _error, change) => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: conversationListsKey(tenant) }),
        // A deleted conversation's page is left right away (it would be a 404 now): its copy is
        // only marked stale, so it is read again if it is ever opened, not while it is left.
        queryClient.invalidateQueries({
          queryKey: conversationKey(tenant, change.conversationId),
          ...(change.kind === "delete" ? { refetchType: "none" as const } : {}),
        }),
      ]);
    },
  });
}
