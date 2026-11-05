import { useMutation, useQueryClient } from "@tanstack/react-query";
import { CircleAlertIcon, InfoIcon } from "lucide-react";
import { useRef, useState } from "react";

import { useMachineApi } from "../../api/pending/MachineApiProvider.tsx";
import {
  isServiceUnavailable,
  type Machine,
  type ProductionRun,
  type RunOverlap,
  runOverlap,
} from "../../api/pending/machines.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { fromLocalInput, toLocalInput } from "../device/time.ts";
import { parseTarget } from "./ChangeoverDialog.tsx";
import { FormDialog } from "./FormDialog.tsx";
import { useMachineFormat } from "./format.ts";
import { UnavailableNotice } from "./parts.tsx";
import { machinesKey } from "./queries.ts";

/** A record entered afterwards, or one that is corrected (`run`). */
export type RunForm = { kind: "backfill" } | { kind: "correct"; run: ProductionRun };

/**
 * The form of docs/design/ms4-ms5/machines.html, state 5: part number, start, end, work order,
 * target and reason. A correction needs a reason and a backfill does not (design decision Q9). A
 * time that overlaps another record is refused by the server (one record at a time per machine);
 * the dialog names the record it ran into and offers to correct that one (`onCorrectOverlap`).
 */
export function RunFormDialog({
  machine,
  form,
  onClose,
  onDone,
  onCorrectOverlap,
}: {
  machine: Machine;
  form: RunForm;
  onClose(): void;
  onDone(): void;
  onCorrectOverlap(overlap: RunOverlap): void;
}) {
  const { t, errorMessage, fieldError } = useI18n();
  const format = useMachineFormat();
  const api = useMachineApi();
  const queryClient = useQueryClient();
  const tenantId = useSession().session?.activeTenant?.id ?? "";
  const partRef = useRef<HTMLInputElement>(null);
  const timeZone = format.timeZone;
  const run = form.kind === "correct" ? form.run : null;

  const [part, setPart] = useState(run?.part_number ?? "");
  const [start, setStart] = useState(run ? toLocalInput(Date.parse(run.started_at), timeZone) : "");
  const [end, setEnd] = useState(run?.ended_at ? toLocalInput(Date.parse(run.ended_at), timeZone) : "");
  const [workOrder, setWorkOrder] = useState(run?.work_order ?? "");
  const [target, setTarget] = useState(run?.target_quantity === null || !run ? "" : String(run.target_quantity));
  const [reason, setReason] = useState("");
  const [submitted, setSubmitted] = useState(false);

  const startAt = fromLocalInput(start, timeZone);
  const endAt = fromLocalInput(end, timeZone);
  // The record in progress keeps no end; a finished one and a backfill need one.
  const endOptional = run !== null && run.ended_at === null;
  const targetValue = parseTarget(target);

  const mutation = useMutation({
    mutationFn: () => {
      const common = {
        part_number: part.trim(),
        work_order: workOrder.trim() === "" ? null : workOrder.trim(),
        target_quantity: targetValue ?? null,
      };
      if (run !== null) {
        return api.correctRun(machine.machine_id, run.run_id, {
          ...common,
          started_at: new Date(startAt ?? 0).toISOString(),
          ended_at: endAt === null ? null : new Date(endAt).toISOString(),
          reason: reason.trim(),
        });
      }
      return api.backfillRun(machine.machine_id, {
        ...common,
        started_at: new Date(startAt ?? 0).toISOString(),
        ended_at: new Date(endAt ?? 0).toISOString(),
        reason: reason.trim() === "" ? null : reason.trim(),
      });
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: machinesKey(tenantId) });
      onDone();
    },
  });

  const overlap = runOverlap(mutation.error);
  const problems = {
    part: part.trim() === "" ? t("machines.changeover.part_required") : undefined,
    start: startAt === null ? t("machines.run.time_invalid") : undefined,
    end:
      endAt === null && !endOptional
        ? t("machines.run.time_invalid")
        : startAt !== null && endAt !== null && endAt <= startAt
          ? t("machines.run.end_before_start")
          : undefined,
    target: targetValue === undefined ? t("machines.form.target_invalid") : undefined,
    reason: run !== null && reason.trim() === "" ? t("machines.run.reason_required") : undefined,
  };
  const submit = () => {
    setSubmitted(true);
    if (Object.values(problems).some((problem) => problem !== undefined)) return;
    mutation.mutate();
  };
  const shown = <K extends keyof typeof problems>(name: K) => (submitted ? problems[name] : undefined);

  const range = (from: string, to: string | null) =>
    to === null
      ? t("machines.run.range_open", { from: format.when(from, Date.now()) })
      : t("machines.run.range", { from: format.when(from, Date.now()), to: format.when(to, Date.now()) });
  const overlapText = overlap
    ? t("machines.run.overlap_range", {
        part: overlap.partNumber,
        range: range(overlap.startedAt, overlap.endedAt),
      })
    : undefined;

  return (
    <FormDialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      title={
        run
          ? t("machines.run.correct_title", { machine: machine.name })
          : t("machines.run.backfill_title", { machine: machine.name })
      }
      description={
        <>
          <p>{run ? t("machines.run.correct_description") : t("machines.run.backfill_description")}</p>
          <p>{t("machines.run.time_zone", { zone: timeZone })}</p>
        </>
      }
      wide
      initialFocus={partRef}
      pending={mutation.isPending}
      submitLabel={t("machines.save")}
      pendingLabel={t("machines.saving")}
      onSubmit={submit}
    >
      {overlap ? (
        <Alert variant="destructive" className="grid-cols-[1rem_1fr]">
          <CircleAlertIcon aria-hidden="true" />
          <AlertTitle>{t("machines.run.overlap_title")}</AlertTitle>
          <AlertDescription>
            <p>{t("machines.run.overlap_body", { part: overlap.partNumber, range: range(overlap.startedAt, overlap.endedAt) })}</p>
            {overlap.runId !== run?.run_id ? (
              <Button type="button" variant="link" onClick={() => onCorrectOverlap(overlap)}>
                {t("machines.run.overlap_fix")}
              </Button>
            ) : null}
          </AlertDescription>
        </Alert>
      ) : mutation.isError ? (
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
      <FormField label={t("machines.changeover.part")} error={shown("part") ?? fieldError(mutation.error, "/part_number")}>
        {(control) => (
          <Input
            {...control}
            ref={partRef}
            value={part}
            autoComplete="off"
            spellCheck={false}
            className="font-mono"
            onChange={(event) => setPart(event.target.value)}
          />
        )}
      </FormField>
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField
          label={t("machines.run.start")}
          error={shown("start") ?? (overlapText ? undefined : fieldError(mutation.error, "/started_at"))}
        >
          {(control) => (
            <Input
              {...control}
              {...(overlap ? { "aria-invalid": true as const } : {})}
              type="datetime-local"
              value={start}
              onChange={(event) => setStart(event.target.value)}
            />
          )}
        </FormField>
        <FormField
          label={endOptional ? t("machines.run.end_optional") : t("machines.run.end")}
          description={endOptional ? t("machines.run.end_in_progress") : undefined}
          error={shown("end") ?? overlapText ?? fieldError(mutation.error, "/ended_at")}
        >
          {(control) => (
            <Input
              {...control}
              {...(overlap ? { "aria-invalid": true as const } : {})}
              type="datetime-local"
              value={end}
              onChange={(event) => setEnd(event.target.value)}
            />
          )}
        </FormField>
      </div>
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
        <FormField label={t("machines.form.target")} error={shown("target")}>
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
      <FormField
        label={run ? t("machines.run.reason") : t("machines.run.reason_optional")}
        description={t("machines.run.reason_hint")}
        error={shown("reason") ?? fieldError(mutation.error, "/reason")}
      >
        {(control) => (
          <Input {...control} value={reason} autoComplete="off" onChange={(event) => setReason(event.target.value)} />
        )}
      </FormField>
      <Alert role="note" className="grid-cols-[1rem_1fr] bg-muted/40">
        <InfoIcon aria-hidden="true" />
        <AlertDescription>
          <p>{t("machines.run.recalculate_note")}</p>
        </AlertDescription>
      </Alert>
    </FormDialog>
  );
}
