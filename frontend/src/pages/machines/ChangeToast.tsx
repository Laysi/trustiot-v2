import { Link } from "react-router";

import type { ChangeoverResult } from "../../api/pending/machines.ts";
import { machinePath } from "../../app/paths.ts";
import { focusRing } from "../../components/ui/button.tsx";
import { cn } from "../../lib/utils.ts";
import type { MachineFormat } from "./format.ts";

/** What a changeover or a start returned, and which machine it was on. */
export interface ToastRecord {
  machineId: string;
  result: ChangeoverResult;
}

/**
 * The toast's sentence and its link (machines.html, state 4): "Changed over: A → B, started
 * 14:32", and "Correct", which opens the new record. The design has no "undo": taking back a
 * record that was made is a correction, and corrections leave a change record.
 */
export function ChangeToast({ done, name, format }: { done: ToastRecord; name: string; format: MachineFormat }) {
  const { t } = format;
  const { previous, current } = done.result;
  const time = format.when(current.started_at, Date.parse(current.started_at));
  return (
    <>
      <p>
        {previous
          ? t("machines.toast.changed", { from: previous.part_number, to: current.part_number, time })
          : t("machines.toast.started", { part: current.part_number, time })}
      </p>
      <Link
        to={machinePath(done.machineId, current.run_id)}
        aria-label={t("machines.toast.correct_for", { machine: name })}
        className={cn("rounded-sm font-medium text-primary underline underline-offset-4", focusRing)}
      >
        {t("machines.toast.correct")}
      </Link>
    </>
  );
}
