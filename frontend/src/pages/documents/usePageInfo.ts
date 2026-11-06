import { useQuery } from "@tanstack/react-query";

import { useDocumentApi } from "../../api/pending/DocumentApiProvider.tsx";
import { useSession } from "../../session/SessionProvider.tsx";
import { pageKey } from "./queries.ts";

/**
 * One page's image address and size. The page view and its image both read it, under one key, so
 * the request is made once and the page knows, as the image does, whether the cited area fits.
 */
export function usePageInfo(documentId: string, version: number, page: number, enabled = true) {
  const api = useDocumentApi();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useQuery({
    queryKey: pageKey(tenant, documentId, version, page),
    queryFn: ({ signal }) => api.getPage({ documentId, version, page }, signal),
    enabled,
  });
}
