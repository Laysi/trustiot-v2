/**
 * What the weekly report's form leaves in the navigation state when it was opened from the setup
 * checklist (Laysi/trustiot-v2#129) and saved: the checklist's step "choose a first use" is done.
 * The conversation home reads it to tick the step.
 */
export interface ChecklistStepState {
  checklistStepDone: "first_use";
}

export const FIRST_USE_DONE: ChecklistStepState = { checklistStepDone: "first_use" };

/** Whether the navigation `state` the home page arrived with says the weekly report was saved from the checklist. */
export function isFirstUseDone(state: unknown): boolean {
  return (
    typeof state === "object" &&
    state !== null &&
    "checklistStepDone" in state &&
    (state as ChecklistStepState).checklistStepDone === "first_use"
  );
}
