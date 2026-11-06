import { useQueryClient } from "@tanstack/react-query";

import type { TenantStateSnapshot, TenantSummary } from "../../../api/pending/platform.ts";
import { platformKey } from "../queries.ts";
import { QuotaDialog } from "./QuotaDialog.tsx";
import { ResumeDialog } from "./ResumeDialog.tsx";
import { SuspendDialog } from "./SuspendDialog.tsx";

export type TenantDialogKind = "suspend" | "reason" | "resume" | "quota";

export interface TenantDialogTarget {
  kind: TenantDialogKind;
  /** The tenant as the page showed it when the person opened the dialog. */
  tenant: TenantSummary;
}

/**
 * The dialogs of a tenant's actions, for the list and the tenant's page: whichever `target` names
 * is open. After a change every back office query is refreshed from the server, so the page shows
 * what was written, not what was hoped (design.md, 「互動與狀態的共同規則」).
 */
export function TenantDialogs(props: {
  target: TenantDialogTarget | null;
  onClose(): void;
  onDone(message: string): void;
  /** The state changed under an open dialog and the person wants to see it. */
  onStale?(current: TenantStateSnapshot): void;
  returnFocus?: () => HTMLElement | null;
}) {
  const queryClient = useQueryClient();
  const { target } = props;
  const done = (message: string) => {
    void queryClient.invalidateQueries({ queryKey: platformKey });
    props.onClose();
    props.onDone(message);
  };
  const stale = (current: TenantStateSnapshot) => {
    void queryClient.invalidateQueries({ queryKey: platformKey });
    props.onClose();
    props.onStale?.(current);
  };
  const onOpenChange = (open: boolean) => {
    if (!open) props.onClose();
  };
  const common = { onOpenChange, onDone: done, ...(props.returnFocus ? { returnFocus: props.returnFocus } : {}) };
  return (
    <>
      {target && (target.kind === "suspend" || target.kind === "reason") ? (
        <SuspendDialog {...common} tenant={target.tenant} mode={target.kind} open onStale={stale} />
      ) : null}
      {target?.kind === "resume" ? <ResumeDialog {...common} tenant={target.tenant} open onStale={stale} /> : null}
      {target?.kind === "quota" ? <QuotaDialog {...common} tenant={target.tenant} open /> : null}
    </>
  );
}
