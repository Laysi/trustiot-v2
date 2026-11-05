import type { ReportKind, ReportScope } from "../../api/pending/reports.ts";
import type { MessageId } from "../../i18n/messages.ts";
import { monthDay } from "./schedule-time.ts";

/** The carbon factor the design states (checklist J5): kgCO2e per kWh. */
export const CARBON_FACTOR = 0.494;

export const KIND_NAME: Record<ReportKind, MessageId> = {
  weekly_energy: "reports.kind.weekly_energy",
  monthly_carbon: "reports.kind.monthly_carbon",
};

export const KIND_DESCRIPTION: Record<ReportKind, MessageId> = {
  weekly_energy: "reports.kind_desc.weekly_energy",
  monthly_carbon: "reports.kind_desc.monthly_carbon",
};

export function isReportKind(value: string | null): value is ReportKind {
  return value === "weekly_energy" || value === "monthly_carbon";
}

/** `MM-DD` inside `currentYear`, `YYYY-MM-DD` outside it (design 「時間」). */
export function shortDate(date: string, currentYear: number): string {
  return Number(date.slice(0, 4)) === currentYear ? monthDay(date) : date;
}

export function periodText(start: string, end: string, currentYear: number): string {
  return `${shortDate(start, currentYear)} – ${shortDate(end, currentYear)}`;
}

/** Whether `scope` names devices. */
export function scopeDeviceCount(scope: ReportScope): number | null {
  return scope.kind === "devices" ? scope.device_ids.length : null;
}
