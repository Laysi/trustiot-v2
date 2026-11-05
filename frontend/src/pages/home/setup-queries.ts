import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import type { SetupApi, SetupState, SetupStepId, SetupUseId } from "../../api/pending/setup.ts";

/** The checklist's query key; it starts with the tenant's ID, as every page's does (a switch resets them). */
export const setupKey = (tenant: string) => ["setup", tenant] as const;

/** How often the page asks again while it waits for the first reading, which nothing the user does can bring. */
export const WAITING_POLL_MS = 30_000;

export function currentStep(state: SetupState) {
  return state.steps.find((step) => step.status === "todo");
}

export function useSetupState(api: SetupApi, tenant: string) {
  return useQuery({
    queryKey: setupKey(tenant),
    queryFn: ({ signal }) => api.getSetup(signal),
    refetchInterval: (query) =>
      query.state.data && currentStep(query.state.data)?.id === "first_data" ? WAITING_POLL_MS : false,
  });
}

/** What a write changes: a step to skip or take back, the first use to choose, or the closing notice. */
export type SetupAction =
  | { kind: "skip"; step: SetupStepId }
  | { kind: "restore"; step: SetupStepId }
  | { kind: "choose"; use: SetupUseId }
  | { kind: "dismiss" };

/**
 * The checklist's writes. Each answers with the state after it, which is what the page then shows:
 * nothing changes on screen before the server said so, so a failure leaves the step as it was.
 */
export function useSetupAction(api: SetupApi, tenant: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (action: SetupAction) => {
      switch (action.kind) {
        case "skip":
          return api.skipStep(action.step);
        case "restore":
          return api.restoreStep(action.step);
        case "choose":
          return api.chooseUse(action.use);
        case "dismiss":
          return api.dismissCompletion();
      }
    },
    onSuccess: (state) => queryClient.setQueryData(setupKey(tenant), state),
    // Whatever the outcome the server is asked again, so a step another admin changed in the meantime shows.
    onSettled: () => queryClient.invalidateQueries({ queryKey: setupKey(tenant) }),
  });
}
