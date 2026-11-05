import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useRef, useState } from "react";

import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import { isServiceUnavailable, type Machine } from "../../api/pending/machines.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { FormDialog } from "./FormDialog.tsx";
import { UnavailableNotice } from "./parts.tsx";
import { machinesKey } from "./queries.ts";

/**
 * Adds a machine, or edits `machine` (name, description, and whether it is a utility such as an
 * air compressor, which usually keeps no production records). The design draws the buttons but
 * not this form, so it is kept to those three fields.
 */
export function MachineFormDialog({
  machine,
  onClose,
  onDone,
}: {
  machine?: Machine;
  onClose(): void;
  onDone(saved: Machine): void;
}) {
  const { t, errorMessage, fieldError } = useI18n();
  const api = useMachineApi();
  const queryClient = useQueryClient();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const nameRef = useRef<HTMLInputElement>(null);
  const [name, setName] = useState(machine?.name ?? "");
  const [description, setDescription] = useState(machine?.description ?? "");
  const [utility, setUtility] = useState(machine?.utility ?? false);
  const [submitted, setSubmitted] = useState(false);

  const mutation = useMutation({
    mutationFn: () => {
      const input = { name: name.trim(), description: description.trim() === "" ? null : description.trim(), utility };
      return machine ? api.updateMachine(machine.machine_id, input) : api.createMachine(input);
    },
    onSuccess: async (saved) => {
      await queryClient.invalidateQueries({ queryKey: machinesKey(tenantId) });
      onDone(saved);
    },
  });
  const nameError =
    (submitted && name.trim() === "" ? t("machines.form.name_required") : undefined) ??
    fieldError(mutation.error, "/name");

  return (
    <FormDialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={machine ? t("machines.form.edit_title") : t("machines.form.create_title")}
      description={<p>{t("machines.form.description")}</p>}
      initialFocus={nameRef}
      pending={mutation.isPending}
      submitLabel={t("machines.save")}
      pendingLabel={t("machines.saving")}
      onSubmit={() => {
        setSubmitted(true);
        if (name.trim() !== "") mutation.mutate();
      }}
    >
      {mutation.isError && !fieldError(mutation.error, "/name") ? (
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
      <FormField label={t("machines.form.name")} error={nameError}>
        {(control) => (
          <Input {...control} ref={nameRef} value={name} autoComplete="off" onChange={(event) => setName(event.target.value)} />
        )}
      </FormField>
      <FormField label={t("machines.form.description_label")}>
        {(control) => (
          <Input
            {...control}
            value={description}
            autoComplete="off"
            onChange={(event) => setDescription(event.target.value)}
          />
        )}
      </FormField>
      <label className="flex items-start gap-2 text-sm">
        <input
          type="checkbox"
          checked={utility}
          className="mt-0.5 size-4 shrink-0 accent-primary"
          onChange={(event) => setUtility(event.target.checked)}
        />
        <span className="grid gap-0.5">
          <span className="font-medium">{t("machines.form.utility")}</span>
          <span className="text-muted-foreground">{t("machines.form.utility_hint")}</span>
        </span>
      </label>
    </FormDialog>
  );
}
