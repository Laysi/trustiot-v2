import { useMutation, useQueryClient } from "@tanstack/react-query";
import { PlusIcon, Trash2Icon } from "lucide-react";
import { useState } from "react";

import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import {
  isServiceUnavailable,
  type MachineRelation,
  type MachineSummary,
  type RelationKind,
} from "../../api/pending/machines.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { FormDialog } from "./FormDialog.tsx";
import { UnavailableNotice } from "./parts.tsx";
import { machinesKey } from "./queries.ts";

/** How a relation reads from the machine's side: `power_in` is "X powers this one". */
export const RELATION_ROLES = ["power_in", "power_out", "drive_in", "drive_out", "flow_up", "flow_down"] as const;
export type RelationRole = (typeof RELATION_ROLES)[number];

export function roleOf(relation: MachineRelation, machineId: string): RelationRole {
  const incoming = relation.to_machine_id === machineId;
  switch (relation.kind) {
    case "power":
      return incoming ? "power_in" : "power_out";
    case "drive":
      return incoming ? "drive_in" : "drive_out";
    case "flow":
      return incoming ? "flow_up" : "flow_down";
  }
}

function toRelation(role: RelationRole, machineId: string, otherId: string): Omit<MachineRelation, "relation_id"> {
  const kind: RelationKind = role.startsWith("power") ? "power" : role.startsWith("drive") ? "drive" : "flow";
  const incoming = role.endsWith("_in") || role === "flow_up";
  return incoming
    ? { kind, from_machine_id: otherId, to_machine_id: machineId }
    : { kind, from_machine_id: machineId, to_machine_id: otherId };
}

interface Row {
  key: number;
  role: RelationRole;
  otherId: string;
}

/** Edits the machine's relations: power, drive and the process before and after (field-context.md, point 1). */
export function RelationsDialog({
  machineId,
  machineName,
  relations,
  others,
  onClose,
  onDone,
}: {
  machineId: string;
  machineName: string;
  relations: readonly MachineRelation[];
  /** The other machines and utilities a relation can lead to. */
  others: readonly MachineSummary[];
  onClose(): void;
  onDone(): void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useMachineApi();
  const queryClient = useQueryClient();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const [next, setNext] = useState(relations.length);
  const [rows, setRows] = useState<Row[]>(() =>
    relations.map((relation, key) => ({
      key,
      role: roleOf(relation, machineId),
      otherId: relation.to_machine_id === machineId ? relation.from_machine_id : relation.to_machine_id,
    })),
  );
  const candidates = others.filter((other) => other.machine_id !== machineId);

  const mutation = useMutation({
    mutationFn: () => {
      const seen = new Set<string>();
      const wanted = rows
        .filter((row) => row.otherId !== "")
        .map((row) => toRelation(row.role, machineId, row.otherId))
        .filter((relation) => {
          const id = `${relation.kind}:${relation.from_machine_id}:${relation.to_machine_id}`;
          if (seen.has(id)) return false;
          seen.add(id);
          return true;
        });
      return api.setRelations(machineId, wanted);
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: machinesKey(tenantId) });
      onDone();
    },
  });
  const update = (key: number, change: Partial<Row>) =>
    setRows((current) => current.map((row) => (row.key === key ? { ...row, ...change } : row)));

  return (
    <FormDialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={t("machines.relations.title", { machine: machineName })}
      description={<p>{t("machines.relations.description")}</p>}
      wide
      pending={mutation.isPending}
      submitLabel={t("machines.save")}
      pendingLabel={t("machines.saving")}
      onSubmit={() => mutation.mutate()}
    >
      {mutation.isError ? (
        isServiceUnavailable(mutation.error) ? (
          <UnavailableNotice />
        ) : (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(mutation.error)}</p>
            </AlertDescription>
          </Alert>
        )
      ) : null}
      {rows.length === 0 ? <p className="text-sm text-muted-foreground">{t("machines.relations.none")}</p> : null}
      <ul className="grid gap-3">
        {rows.map((row, index) => (
          <li key={row.key} className="grid grid-cols-[1fr_auto] items-end gap-2 sm:grid-cols-[1fr_1fr_auto]">
            <div className="grid gap-1">
              <label htmlFor={`relation-kind-${row.key}`} className="text-xs text-muted-foreground">
                {t("machines.relations.kind", { number: index + 1 })}
              </label>
              <NativeSelect
                id={`relation-kind-${row.key}`}
                value={row.role}
                onChange={(event) => update(row.key, { role: event.target.value as RelationRole })}
              >
                {RELATION_ROLES.map((role) => (
                  <option key={role} value={role}>
                    {t(`machines.relations.role.${role}`)}
                  </option>
                ))}
              </NativeSelect>
            </div>
            <div className="col-span-2 grid gap-1 sm:col-span-1 sm:col-start-2">
              <label htmlFor={`relation-machine-${row.key}`} className="text-xs text-muted-foreground">
                {t("machines.relations.machine", { number: index + 1 })}
              </label>
              <NativeSelect
                id={`relation-machine-${row.key}`}
                value={row.otherId}
                onChange={(event) => update(row.key, { otherId: event.target.value })}
              >
                <option value="">{t("machines.relations.choose")}</option>
                {candidates.map((other) => (
                  <option key={other.machine_id} value={other.machine_id}>
                    {other.name}
                  </option>
                ))}
              </NativeSelect>
            </div>
            <Button
              type="button"
              variant="ghost"
              size="icon"
              aria-label={t("machines.relations.remove", { number: index + 1 })}
              className="row-start-1 col-start-2 sm:row-start-auto sm:col-start-3"
              onClick={() => setRows((current) => current.filter((item) => item.key !== row.key))}
            >
              <Trash2Icon aria-hidden="true" />
            </Button>
          </li>
        ))}
      </ul>
      <div>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => {
            setRows((current) => [...current, { key: next, role: "flow_up", otherId: "" }]);
            setNext(next + 1);
          }}
        >
          <PlusIcon aria-hidden="true" />
          {t("machines.relations.add")}
        </Button>
      </div>
    </FormDialog>
  );
}
