/**
 * What the weekly report's form leaves in the navigation state when it was opened from the setup
 * checklist (Laysi/trustiot-v2#129) and saved: the checklist's step "choose a first use" is done.
 * The conversation home reads it to tick the step.
 */
export interface ChecklistStepState {
  checklistStepDone: "first_use";
}

export const FIRST_USE_DONE: ChecklistStepState = { checklistStepDone: "first_use" };
