import { useMutation, useQueryClient } from "@tanstack/react-query";

import { useAlertsApi } from "../../api/pending/AlertsApiProvider.tsx";
import { alertsKey } from "./queries.ts";
import { useTenantId } from "./state.ts";

/** Runs a write and reads everything again afterwards, whether or not it succeeded: what is shown is what the server holds. */
function useAlertsWrite<Variables>(write: (variables: Variables) => Promise<void>) {
  const queryClient = useQueryClient();
  const tenant = useTenantId();
  return useMutation({
    mutationFn: write,
    onSettled: () => queryClient.invalidateQueries({ queryKey: alertsKey(tenant) }),
  });
}

export function useDisableRule() {
  const api = useAlertsApi();
  return useAlertsWrite((request: { ruleId: string; reason?: string }) =>
    api.disableRule(request.ruleId, request.reason),
  );
}

export function useEnableRule() {
  const api = useAlertsApi();
  return useAlertsWrite((ruleId: string) => api.enableRule(ruleId));
}

export function usePauseAiRules() {
  const api = useAlertsApi();
  return useAlertsWrite((_: void) => api.pauseAiRules());
}

export function useResumeAiRules() {
  const api = useAlertsApi();
  return useAlertsWrite((_: void) => api.resumeAiRules());
}

export function useMarkRead() {
  const api = useAlertsApi();
  return useAlertsWrite((id: string) => api.markRead(id));
}

export function useMarkAllRead() {
  const api = useAlertsApi();
  return useAlertsWrite((_: void) => api.markAllRead());
}
