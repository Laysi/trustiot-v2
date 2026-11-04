import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import type { VersionCreated } from "../../api/dashboards.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { dashboardListsKey, dashboardViewsKey, versionListKey } from "../dashboards/queries.ts";

// A conversation's drafts. The key starts with the active tenant's ID like the other pages' keys
// (a cached answer is never shown for another tenant), and is the user's own: a conversation only
// belongs to who started it.

/** One conversation's drafts. */
export const conversationDraftsKey = (tenant: string, conversationId: string) =>
  ["conversations", tenant, "drafts", conversationId] as const;

/**
 * The conversation's drafts, the most recently updated first. Only the first page (50) is read:
 * the panel shows the latest draft and offers the others by title, and a conversation does not
 * make that many.
 */
export function useConversationDrafts(conversationId: string) {
  const api = useDashboardApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useQuery({
    queryKey: conversationDraftsKey(tenant, conversationId),
    queryFn: async ({ signal }) => (await api.listConversationDrafts(conversationId, undefined, signal)).dashboards,
  });
}

/**
 * Saves a draft's version to the dashboard list. Afterwards, whether or not it worked, the drafts
 * (a published draft's saved dashboard changes) and the list are read again, and so are the saved
 * dashboard's own views and versions: another member may have changed it, which is what a refusal
 * for a stale base means. The draft's dataset runs belong to the viewing and are left alone.
 */
export function usePublishDraft(
  conversationId: string,
  /** Told when a version was saved, even if the panel that asked has been left since. */
  onSaved: (draftId: string, version: number, created: VersionCreated) => void,
) {
  const api = useDashboardApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useMutation({
    mutationFn: ({ draftId, version }: { draftId: string; version: number }) => api.publish(draftId, { version }),
    onSuccess: (created, { draftId, version }) => onSaved(draftId, version, created),
    onSettled: async (created, _error, { draftId }) => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: conversationDraftsKey(tenant, conversationId) }),
        queryClient.invalidateQueries({ queryKey: dashboardViewsKey(tenant, draftId) }),
        queryClient.invalidateQueries({ queryKey: dashboardListsKey(tenant) }),
        ...(created
          ? [
              queryClient.invalidateQueries({ queryKey: dashboardViewsKey(tenant, created.dashboard_id) }),
              queryClient.invalidateQueries({ queryKey: versionListKey(tenant, created.dashboard_id) }),
            ]
          : []),
      ]);
    },
  });
}
