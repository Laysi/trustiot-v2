// Pending Laysi/trustiot-v2#173: replace with a module over the generated client when api/openapi.json has these endpoints.
//
// The report endpoints the reports screens (Laysi/trustiot-v2#174) use. The backend does not
// exist yet, so paths, fields and rules come from the issue's 範圍 and docs/design/ms4-ms5/
// (decisions Q6 and Q7), and the assumptions are listed in the pull request of Laysi/trustiot-v2#174:
//
// - Reports are a Business-and-above entitlement (docs/product/plans.md). A tenant without it
//   gets 403 `permission_denied` with message key `entitlement.feature_not_in_plan` from every
//   endpoint (the shape `crates/contract/src/entitlements.rs` defines); the page shows the
//   upgrade wall for it.
// - Creating, changing, deleting and running a schedule, and retrying a report, need an editor
//   (design 「角色」); reading and downloading need any member.
// - A report keeps the dataset of the moment it was made (Q6): its numbers never change later.

/** The two report types: last week's energy per device, and last month's energy and carbon. */
export const REPORT_KINDS = ["weekly_energy", "monthly_carbon"] as const;
export type ReportKind = (typeof REPORT_KINDS)[number];

/** Which devices a report covers. */
export type ReportScope = { kind: "all" } | { kind: "devices"; device_ids: string[] };

/**
 * A schedule's settings. `send_on` is the weekday (1 Monday to 7 Sunday) of a weekly report and
 * the day of the month (1 to 28) of a monthly one; `time` is `HH:MM` in `time_zone`, an IANA name.
 * `recipients` are Email addresses: members' or outside the organization's (Q7).
 */
export interface ScheduleInput {
  kind: ReportKind;
  scope: ReportScope;
  send_on: number;
  time: string;
  time_zone: string;
  recipients: string[];
}

/** How the last run of a schedule ended. `period_*` are the dates (`YYYY-MM-DD`) the report covered. */
export interface ScheduleRun {
  period_start: string;
  period_end: string;
  at: string;
  outcome: "sent" | "failed";
}

export interface ReportSchedule extends ScheduleInput {
  id: string;
  enabled: boolean;
  /** When the next report is made; `null` while the schedule is off. */
  next_run_at: string | null;
  last_run: ScheduleRun | null;
  /** Recipients whose mail bounced at the last send; the others received it. */
  bounced_recipients: string[];
}

export type ReportStatus = "ready" | "generating" | "failed";

/** Why a report could not be made. */
export type ReportFailure = "query_timeout" | "internal";

/** One report in the list. `period_start` and `period_end` are inclusive dates (`YYYY-MM-DD`). */
export interface ReportSummary {
  id: string;
  /** The schedule that made it; `null` for one that was asked for by hand and kept. */
  schedule_id: string | null;
  kind: ReportKind;
  period_start: string;
  period_end: string;
  status: ReportStatus;
  /** When it was made; `null` while it is being made. */
  generated_at: string | null;
  failure: ReportFailure | null;
}

/** The headline numbers. A change is a ratio (0.042 is +4.2 %); `null` when there is nothing to compare with. */
export interface ReportKpis {
  total_kwh: number;
  carbon_kg: number;
  /** kgCO2e per kWh (0.494, checklist J5). */
  carbon_factor: number;
  previous_period_kwh: number | null;
  previous_period_change: number | null;
  /** Only a monthly report compares with the same month of last year. */
  previous_year_kwh: number | null;
  previous_year_change: number | null;
}

export interface ReportDay {
  /** `YYYY-MM-DD`. */
  date: string;
  kwh: number;
}

export interface ReportDevice {
  device_id: string;
  name: string;
  kwh: number;
  carbon_kg: number;
  change: number | null;
}

/** The stored dataset of a report: what the page draws and what the CSV holds. */
export interface ReportContent {
  /** The IANA time zone the periods and days are in. */
  time_zone: string;
  scope: ReportScope;
  kpis: ReportKpis;
  daily: ReportDay[];
  devices: ReportDevice[];
}

/** A report with its content, which is `null` until it is ready. */
export interface ReportDetail extends ReportSummary {
  content: ReportContent | null;
}

export interface ReportList {
  reports: ReportSummary[];
  next_cursor: string | null;
}

export type DownloadFormat = "pdf" | "csv";

export interface ReportApi {
  /** The tenant's schedules, in the order they were made. */
  listSchedules(signal?: AbortSignal): Promise<ReportSchedule[]>;
  getSchedule(scheduleId: string, signal?: AbortSignal): Promise<ReportSchedule>;
  /** Editors and admins; a recipient that is not an Email address is 422 with the field error `/recipients`. */
  createSchedule(input: ScheduleInput): Promise<ReportSchedule>;
  updateSchedule(scheduleId: string, input: ScheduleInput): Promise<ReportSchedule>;
  setScheduleEnabled(scheduleId: string, enabled: boolean): Promise<ReportSchedule>;
  deleteSchedule(scheduleId: string): Promise<void>;
  /** Makes one report for the schedule's current period now; it comes back `generating`. */
  generateNow(scheduleId: string): Promise<ReportSummary>;
  /** One page of reports, the newest first; `kind` narrows to one type. */
  listReports(query: { kind?: ReportKind; cursor?: string; limit?: number }, signal?: AbortSignal): Promise<ReportList>;
  getReport(reportId: string, signal?: AbortSignal): Promise<ReportDetail>;
  /** Makes a failed report again. */
  retryReport(reportId: string): Promise<ReportSummary>;
  /** The report's file. The file's name is the page's choice, never the server's. */
  download(reportId: string, format: DownloadFormat, signal?: AbortSignal): Promise<Blob>;
}
