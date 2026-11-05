import { useMutation, useQueryClient } from "@tanstack/react-query";
import { SquareIcon } from "lucide-react";

import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import type { Machine, ProductionRun } from "../../api/pending/machines.ts";
import { Button, type ButtonProps } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { ConfirmAction } from "../dashboards/ConfirmAction.tsx";
import { machinesKey } from "./queries.ts";

/**
 * "End production", confirmed first: the server notes the moment of the press as the end, and a
 * mistake is only fixed by a correction. The dialog names the part that ends.
 */
export function EndRunAction({
  machine,
  run,
  variant = "outline",
  onDone,
}: {
  machine: Machine;
  run: ProductionRun;
  variant?: ButtonProps["variant"];
  onDone?(ended: ProductionRun): void;
}) {
  const { t } = useI18n();
  const api = useMachineApi();
  const queryClient = useQueryClient();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const mutation = useMutation({
    mutationFn: () => api.endRun(machine.machine_id),
    onSuccess: async (ended) => {
      await queryClient.invalidateQueries({ queryKey: machinesKey(tenantId) });
      onDone?.(ended);
    },
  });
  return (
    <ConfirmAction
      trigger={
        <Button variant={variant} aria-label={t("machines.end.button_for", { machine: machine.name })}>
          <SquareIcon aria-hidden="true" />
          {t("machines.end.button")}
        </Button>
      }
      title={t("machines.end.title", { machine: machine.name })}
      body={t("machines.end.body", { part: run.part_number })}
      confirm={t("machines.end.confirm")}
      pending={t("machines.saving")}
      mutation={mutation}
    />
  );
}
