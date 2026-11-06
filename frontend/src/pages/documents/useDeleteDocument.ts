import { useMutation, useQueryClient } from "@tanstack/react-query";

import { useDocumentApi } from "../../api/pending/DocumentApiProvider.tsx";
import { removeQueriesOnceUnobserved } from "../../app/query-client.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { documentKey, documentListKey } from "./queries.ts";

/**
 * Deletes a document. What the page cached about it goes too (its details and every page image
 * address, which sit under the document's key), so a deleted document's pages are never shown from
 * the cache while the tombstone is on its way, or if that request fails; the list is read again.
 * `onDeleted` runs first, so a page that is showing the document can leave it. The queries go once
 * nothing observes them: the page view is still mounted after its navigation has resolved, and a
 * query removed under it would be built and read again by its next render.
 */
export function useDeleteDocument(onDeleted?: (document: { id: string; name: string }) => void | Promise<void>) {
  const api = useDocumentApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useMutation({
    mutationFn: (document: { id: string; name: string }) => api.deleteDocument(document.id),
    onSuccess: async (_none, document) => {
      await onDeleted?.(document);
      removeQueriesOnceUnobserved(queryClient, documentKey(tenant, document.id));
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: documentListKey(tenant) }),
  });
}
