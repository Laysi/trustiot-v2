import type { ReportKind } from "../../api/pending/reports.ts";

/** The reports pages' paths (docs/design/ms4-ms5/design.md 「深連結」). */
export const REPORTS_PATH = "/reports";
export const NEW_SCHEDULE_PATH = `${REPORTS_PATH}/schedules/new`;

/** A report's page. */
export function reportPath(reportId: string): string {
  return `${REPORTS_PATH}/${encodeURIComponent(reportId)}`;
}

/** The form that changes a schedule. */
export function editSchedulePath(scheduleId: string): string {
  return `${REPORTS_PATH}/schedules/${encodeURIComponent(scheduleId)}/edit`;
}

/** Where the setup checklist's step "choose a first use" sends a new tenant: the weekly report, filled in. */
export const WEEKLY_REPORT_ONBOARDING_PATH = `${NEW_SCHEDULE_PATH}?preset=weekly&from=onboarding`;

/** The new-schedule form for a kind, with the kind chosen. */
export function newScheduleFor(kind: ReportKind): string {
  return `${NEW_SCHEDULE_PATH}?kind=${kind}`;
}
