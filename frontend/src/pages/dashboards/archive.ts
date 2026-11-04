import { useMutation, useQueryClient } from "@tanstack/react-query";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import { useSession } from "../../session/SessionProvider.tsx";
import { dashboardListsKey, dashboardViewsKey } from "./queries.ts";

/**
 * Archives a dashboard or unarchives it (`PUT` and `DELETE …/archive`). It takes effect at once
 * (docs/design/chat-dashboards/design.md, 「列表」); the lists and the dashboard's own view are
 * read again afterwards, whether or not it succeeded, so what is shown is what the server holds.
 */
export function useSetArchived() {
  const api = useDashboardApi();
  const queryClient = useQueryClient();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  return useMutation({
    mutationFn: (request: { dashboardId: string; archived: boolean }) =>
      api.setArchived(request.dashboardId, request.archived),
    onSettled: async (_result, _error, request) => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: dashboardListsKey(tenant) }),
        queryClient.invalidateQueries({ queryKey: dashboardViewsKey(tenant, request.dashboardId) }),
      ]);
    },
  });
}
