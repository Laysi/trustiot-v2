import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { useConversationApi } from "../../../api/ConversationApiProvider.tsx";
import { MAX_PAGE } from "../../../api/conversations.ts";
import { useSession } from "../../../session/SessionProvider.tsx";
import { conversationKey, conversationListsKey } from "../queries.ts";

// The sharing queries (Laysi/trustiot-v2#1048). Like the conversation pages' keys they start with
// the active tenant's ID, and every write reads what it touched again, whether or not it
// succeeded, so what is shown is what the server holds. Whether the owner's conversation is
// shared is `shared_at` of the conversation itself (`conversationKey`).

/** Every query about what others shared. */
const sharedKey = (tenant: string) => ["conversations", tenant, "shared"] as const;

/** The "Shared with me" list. */
export const sharedListKey = (tenant: string) => [...sharedKey(tenant), "list"] as const;

/** One shared conversation, to read. */
export const sharedConversationKey = (tenant: string, conversationId: string) =>
  [...sharedKey(tenant), "detail", conversationId] as const;

/**
 * Shares or unshares the caller's conversation. The conversation, the lists and what others
 * share are read again afterwards, whether or not it succeeded.
 */
export function useShareChange(conversationId: string) {
  const api = useConversationApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useMutation({
    mutationFn: async (share: boolean): Promise<void> => {
      if (share) await api.shareConversation(conversationId);
      else await api.unshareConversation(conversationId);
    },
    onSettled: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: conversationKey(tenant, conversationId) }),
        queryClient.invalidateQueries({ queryKey: conversationListsKey(tenant) }),
        queryClient.invalidateQueries({ queryKey: sharedKey(tenant) }),
      ]);
    },
  });
}

/** What other members shared, page by page (100 a page, the service's most), the most recently active first. */
export function useSharedPages() {
  const api = useConversationApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useInfiniteQuery({
    queryKey: sharedListKey(tenant),
    queryFn: ({ pageParam, signal }) =>
      api.listShared({ limit: MAX_PAGE, ...(pageParam !== undefined ? { cursor: pageParam } : {}) }, signal),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
}

/** One shared conversation, read only. */
export function useSharedConversation(conversationId: string) {
  const api = useConversationApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useQuery({
    queryKey: sharedConversationKey(tenant, conversationId),
    queryFn: ({ signal }) => api.getShared(conversationId, signal),
  });
}

/**
 * Copies a conversation into a new one of the caller's. The caller's conversation lists are read
 * again afterwards, so the copy is in them whether or not the request is known to have worked.
 */
export function useCopyConversation(conversationId: string) {
  const api = useConversationApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useMutation({
    mutationFn: () => api.copyConversation(conversationId),
    onSettled: () => queryClient.invalidateQueries({ queryKey: conversationListsKey(tenant) }),
  });
}
