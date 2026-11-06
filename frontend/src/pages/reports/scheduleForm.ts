import type { ReportKind, ReportSchedule, ScheduleInput } from "../../api/pending/reports.ts";
import { clientProblem, type Problem } from "../form-problems.ts";
import { parseTime } from "./schedule-time.ts";

/** What the schedule form holds while it is being filled in. */
export interface ScheduleFormValues {
  kind: ReportKind;
  scopeMode: "all" | "devices";
  deviceIds: string[];
  /** The weekday (1 to 7) of a weekly report, the day of the month (1 to 28) of a monthly one. */
  sendOn: number;
  time: string;
  recipients: string[];
  /** What is typed in the recipient box and not yet made a chip. */
  recipientDraft: string;
}

/** The pointers (into the request body) the form's problems are about. */
export const POINTERS = { recipients: "/recipients", scope: "/scope", time: "/time" } as const;

export const MAX_RECIPIENTS = 20;
export const DEFAULT_TIME = "08:00";

/** The input a new schedule starts from; the weekly report is the one the setup checklist offers. */
export function newValues(kind: ReportKind, self: string | null): ScheduleFormValues {
  return {
    kind,
    scopeMode: "all",
    deviceIds: [],
    sendOn: 1,
    time: DEFAULT_TIME,
    recipients: self === null ? [] : [self],
    recipientDraft: "",
  };
}

export function valuesOf(schedule: ReportSchedule): ScheduleFormValues {
  return {
    kind: schedule.kind,
    scopeMode: schedule.scope.kind === "all" ? "all" : "devices",
    deviceIds: schedule.scope.kind === "devices" ? [...schedule.scope.device_ids] : [],
    sendOn: schedule.send_on,
    time: schedule.time,
    recipients: [...schedule.recipients],
    recipientDraft: "",
  };
}

/** Whether `text` looks like one Email address: one `@`, a dot in the domain, no spaces, at most 254 characters. */
export function isEmail(text: string): boolean {
  return text.length <= 254 && /^[^\s@]+@[^\s@.]+(\.[^\s@.]+)+$/.test(text);
}

/**
 * Makes what is typed in the recipient box a chip. Nothing typed leaves the values as they are;
 * a text that is not an address, or one beyond the limit, stays in the box with its problem.
 */
export function commitRecipient(values: ScheduleFormValues): { values: ScheduleFormValues; problem: Problem | null } {
  const text = values.recipientDraft.trim().replace(/[,;]$/, "").trim();
  if (text === "") return { values: { ...values, recipientDraft: "" }, problem: null };
  if (!isEmail(text)) {
    return { values, problem: clientProblem(POINTERS.recipients, "reports.recipient_invalid", { email: text }) };
  }
  if (values.recipients.some((email) => email.toLowerCase() === text.toLowerCase())) {
    return { values: { ...values, recipientDraft: "" }, problem: null };
  }
  if (values.recipients.length >= MAX_RECIPIENTS) {
    return {
      values,
      problem: clientProblem(POINTERS.recipients, "reports.form.error.recipients_many", { max: MAX_RECIPIENTS }),
    };
  }
  return { values: { ...values, recipients: [...values.recipients, text], recipientDraft: "" }, problem: null };
}

/** The problems the page can find before sending anything. */
export function validate(values: ScheduleFormValues): Problem[] {
  const problems: Problem[] = [];
  if (values.recipients.length === 0) {
    problems.push(clientProblem(POINTERS.recipients, "reports.form.error.recipients_empty"));
  }
  if (values.scopeMode === "devices" && values.deviceIds.length === 0) {
    problems.push(clientProblem(POINTERS.scope, "reports.form.error.devices_empty"));
  }
  if (parseTime(values.time) === null) {
    problems.push(clientProblem(POINTERS.time, "reports.form.error.time_invalid"));
  }
  return problems;
}

export function toInput(values: ScheduleFormValues, timeZone: string): ScheduleInput {
  return {
    kind: values.kind,
    scope: values.scopeMode === "all" ? { kind: "all" } : { kind: "devices", device_ids: [...values.deviceIds] },
    send_on: values.sendOn,
    time: values.time,
    time_zone: timeZone,
    recipients: [...values.recipients],
  };
}

/** The input a problem's pointer belongs to: `/recipients/1` is about the recipients. */
export function baseOf(pointer: string): string {
  for (const base of Object.values(POINTERS)) {
    if (pointer === base || pointer.startsWith(`${base}/`)) return base;
  }
  return pointer;
}
