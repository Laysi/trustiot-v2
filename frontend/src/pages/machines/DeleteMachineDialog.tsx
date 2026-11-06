import { useMutation, useQueryClient } from "@tanstack/react-query";

import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import type { Machine } from "../../api/pending/machines.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../components/ui/alert-dialog.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { machinesKey } from "./queries.ts";

/**
 * Confirms and deletes a machine. The focus starts on Cancel. Its production records, the change
 * record's lines about them and its data point assignments go with it (field-context.md), so the
 * text says so.
 */
export function DeleteMachineDialog({
  machine,
  onClose,
  onDeleted,
}: {
  machine: Machine;
  onClose(): void;
  onDeleted(): void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useMachineApi();
  const queryClient = useQueryClient();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const mutation = useMutation({
    mutationFn: () => api.deleteMachine(machine.machine_id),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: machinesKey(tenantId) });
      onDeleted();
    },
  });
  return (
    <AlertDialog
      open
      onOpenChange={(open) => {
        if (!open && !mutation.isPending) onClose();
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{t("machines.delete.title", { machine: machine.name })}</AlertDialogTitle>
          <AlertDialogDescription>{t("machines.delete.body")}</AlertDialogDescription>
        </AlertDialogHeader>
        {mutation.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(mutation.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={mutation.isPending}>{t("machines.cancel")}</AlertDialogCancel>
          <Button variant="destructive" disabled={mutation.isPending} onClick={() => mutation.mutate()}>
            {mutation.isPending ? <Spinner /> : null}
            {mutation.isPending ? t("machines.deleting") : t("machines.delete.confirm")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
