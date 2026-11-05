import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ClockIcon, PlayIcon, RepeatIcon } from "lucide-react";
import { useMemo, useRef, useState } from "react";

import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import {
  type ChangeoverResult,
  isServiceUnavailable,
  type Machine,
  type ProductionRun,
} from "../../api/pending/machines.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { focusRing } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { FormDialog } from "./FormDialog.tsx";
import { useMachineFormat } from "./format.ts";
import { UnavailableNotice } from "./parts.tsx";
import { machinesKey, runsKey } from "./queries.ts";

const DAY = 86_400_000;
/** How many part numbers used lately the dialog offers. */
const RECENT_PARTS = 5;

/** A positive whole number from the box, `null` when empty, `undefined` when it is not one. */
export function parseTarget(text: string): number | null | undefined {
  const trimmed = text.trim().replaceAll(",", "");
  if (trimmed === "") return null;
  return /^\d{1,9}$/.test(trimmed) && Number(trimmed) > 0 ? Number(trimmed) : undefined;
}

/**
 * A changeover, or the start of production when nothing is in progress (docs/design/ms4-ms5/
 * machines.html, state 3). The start time is not a field: the server notes the moment of the
 * request (design decision Q10, checklist C5), so a late press is fixed afterwards by a
 * correction, which leaves a change record.
 */
export function ChangeoverDialog({
  machine,
  current,
  onClose,
  onDone,
}: {
  machine: Machine;
  /** The record in progress that this changeover ends; `null` starts production. */
  current: ProductionRun | null;
  onClose(): void;
  onDone(result: ChangeoverResult): void;
}) {
  const { t, errorMessage, fieldError } = useI18n();
  const format = useMachineFormat();
  const api = useMachineApi();
  const queryClient = useQueryClient();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const partRef = useRef<HTMLInputElement>(null);
  const [part, setPart] = useState("");
  const [workOrder, setWorkOrder] = useState("");
  const [target, setTarget] = useState("");
  const [submitted, setSubmitted] = useState(false);
  // The moment the dialog was opened: the page says "now" is this, the server notes its own.
  const [openedAt] = useState(() => Date.now());

  const recent = useQuery({
    queryKey: runsKey(tenantId, machine.machine_id, openedAt - 30 * DAY),
    queryFn: ({ signal }) =>
      api.listRuns(
        machine.machine_id,
        { from: new Date(openedAt - 30 * DAY).toISOString(), to: new Date(openedAt + DAY).toISOString() },
        signal,
      ),
  });
  const recentParts = useMemo(() => {
    const seen: string[] = [];
    for (const run of recent.data ?? []) if (!seen.includes(run.part_number)) seen.push(run.part_number);
    return seen.slice(0, RECENT_PARTS);
  }, [recent.data]);

  const mutation = useMutation({
    mutationFn: () =>
      api.changeover(machine.machine_id, {
        part_number: part.trim(),
        work_order: workOrder.trim() === "" ? null : workOrder.trim(),
        target_quantity: parseTarget(target) ?? null,
      }),
    onSuccess: async (result) => {
      await queryClient.invalidateQueries({ queryKey: machinesKey(tenantId) });
      onDone(result);
    },
  });

  const targetValue = parseTarget(target);
  const partError =
    (submitted && part.trim() === "" ? t("machines.changeover.part_required") : undefined) ??
    fieldError(mutation.error, "/part_number");
  const targetError =
    (targetValue === undefined ? t("machines.form.target_invalid") : undefined) ??
    fieldError(mutation.error, "/target_quantity");
  const submit = () => {
    setSubmitted(true);
    if (part.trim() === "" || targetValue === undefined) return;
    mutation.mutate();
  };
  const now = format.when(new Date(openedAt).toISOString(), openedAt);
  const title = current
    ? t("machines.changeover.title", { machine: machine.name })
    : t("machines.start.title", { machine: machine.name });

  return (
    <FormDialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={title}
      description={
        <p>
          {current
            ? t("machines.changeover.description", {
                part: current.part_number,
                time: format.when(current.started_at, openedAt),
              })
            : t("machines.start.description")}
        </p>
      }
      initialFocus={partRef}
      pending={mutation.isPending}
      submitLabel={current ? t("machines.changeover.confirm") : t("machines.start.confirm")}
      pendingLabel={t("machines.saving")}
      submitIcon={current ? <RepeatIcon aria-hidden="true" /> : <PlayIcon aria-hidden="true" />}
      onSubmit={submit}
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
      <FormField label={t("machines.changeover.part")} error={partError}>
        {(control) => (
          <>
            <Input
              {...control}
              ref={partRef}
              value={part}
              autoComplete="off"
              spellCheck={false}
              className="h-11 font-mono text-lg md:text-lg"
              onChange={(event) => setPart(event.target.value)}
            />
            {recentParts.length > 0 ? (
              <div role="group" aria-label={t("machines.changeover.recent")} className="flex flex-wrap items-center gap-2">
                <span className="text-xs text-muted-foreground">{t("machines.changeover.recent_label")}</span>
                {recentParts.map((recentPart) => (
                  <button
                    key={recentPart}
                    type="button"
                    className={cn(
                      "rounded-full border bg-card px-3 py-1 font-mono text-xs hover:bg-accent",
                      focusRing,
                    )}
                    onClick={() => {
                      setPart(recentPart);
                      partRef.current?.focus();
                    }}
                  >
                    {recentPart}
                  </button>
                ))}
              </div>
            ) : null}
          </>
        )}
      </FormField>
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField label={t("machines.form.work_order")}>
          {(control) => (
            <Input
              {...control}
              value={workOrder}
              autoComplete="off"
              className="font-mono"
              onChange={(event) => setWorkOrder(event.target.value)}
            />
          )}
        </FormField>
        <FormField label={t("machines.form.target")} error={targetError}>
          {(control) => (
            <Input
              {...control}
              value={target}
              inputMode="numeric"
              autoComplete="off"
              onChange={(event) => setTarget(event.target.value)}
            />
          )}
        </FormField>
      </div>
      <Alert role="note" className="grid-cols-[1rem_1fr] bg-muted/40">
        <ClockIcon aria-hidden="true" />
        <AlertDescription>
          <p>{t("machines.changeover.time_note", { time: now })}</p>
        </AlertDescription>
      </Alert>
    </FormDialog>
  );
}
