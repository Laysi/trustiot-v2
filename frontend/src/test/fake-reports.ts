import { type ComponentType, createElement, type ReactNode } from "react";

import { ReportApiProvider } from "../api/pending/ReportApiProvider.tsx";
import type {
  DownloadFormat,
  ReportApi,
  ReportContent,
  ReportDetail,
  ReportKind,
  ReportList,
  ReportSchedule,
  ReportScope,
  ReportSummary,
  ScheduleInput,
} from "../api/pending/reports.ts";
import { addDays, nextOccurrence, weekdayOf } from "../pages/reports/schedule-time.ts";
import type { ProviderPreviewDeclaration } from "../ui-preview/contract.ts";
import { fakeUuid } from "./fake-devices.ts";
import { apiError } from "./fake-identity.ts";

/**
 * The report API held in memory (api/pending/reports.ts: the backend, Laysi/trustiot-v2#173, does
 * not exist yet), by the rules that file states:
 *
 * - `plan: "starter"` has no reports: every endpoint is 403 `entitlement.feature_not_in_plan`.
 * - A schedule needs at least one recipient and every recipient an Email address, else 422 with
 *   the field error `/recipients` (message key `reports.recipient_invalid`, `details.email`).
 * - `generateNow` and `retryReport` leave a `generating` report; `finish` (a test calls it, the
 *   preview calls it after a few seconds) turns every one into `ready`.
 * - The list is the newest period first, `limit` and `cursor` page it; `kind` narrows it.
 * - A report's content is made once, when it is made, and never changes (decision Q6).
 * - The roles are the session's, not the fake's: the API does not refuse by role here.
 *
 * Every call is recorded. `failNext` fails the next call to an endpoint; `hold` keeps calls to
 * the endpoints named waiting until `release`.
 */

type Endpoint = keyof ReportApi;

export interface FakeReportCall {
  endpoint: Endpoint;
  args: unknown[];
}

const DAY_MS = 86_400_000;
export const CARBON_FACTOR = 0.494;
const DEVICE_NAMES = ["line-2-meter", "compressor-1", "chiller-2", "boiler-1"];

/** Where a local wall-clock time in `zone` is on the time line (one correction is enough away from a transition). */
function zonedInstant(date: string, time: string, zone: string): Date {
  const guess = Date.parse(`${date}T${time}:00Z`);
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone: zone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  }).formatToParts(new Date(guess));
  const part = (type: string) => Number(parts.find((item) => item.type === type)?.value ?? 0);
  const shown = Date.UTC(part("year"), part("month") - 1, part("day"), part("hour"), part("minute"));
  return new Date(guess - (shown - guess));
}

function isoDate(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

/** A day's energy: a weekday pattern with a little noise that depends only on the date. */
function dayKwh(date: string, scale: number): number {
  const noise = [...date].reduce((sum, char) => (sum * 31 + char.charCodeAt(0)) % 97, 7) / 97;
  const weekend = weekdayOf(date) >= 6;
  return Math.round(scale * (weekend ? 0.45 : 0.8 + noise * 0.25));
}

/** The dataset of a report: the days of the period, and each device's share. */
export function makeContent(
  kind: ReportKind,
  start: string,
  end: string,
  scope: ReportScope,
  zone: string,
  names: ReadonlyMap<string, string> = new Map(),
): ReportContent {
  // The rows are the devices the report covers: the four of the preview, or the ones chosen.
  const covered =
    scope.kind === "all"
      ? DEVICE_NAMES.map((name) => ({ id: `preview-${name}`, name }))
      : scope.device_ids.map((id) => ({ id, name: names.get(id) ?? id }));
  const daily: ReportContent["daily"] = [];
  for (let date = start; date <= end; date = addDays(date, 1)) {
    daily.push({ date, kwh: dayKwh(date, 140 * covered.length) });
  }
  const total = daily.reduce((sum, day) => sum + day.kwh, 0);
  const shares = [0.42, 0.31, 0.17, 0.1];
  const weights = covered.map((_, index) => shares[index % shares.length] ?? 0.1);
  const weightSum = weights.reduce((sum, weight) => sum + weight, 0) || 1;
  const devices = covered.map((device, index) => {
    const kwh = Math.round((total * (weights[index] ?? 0)) / weightSum);
    return {
      device_id: device.id,
      name: device.name,
      kwh,
      carbon_kg: Math.round(kwh * CARBON_FACTOR),
      change: [0.098, 0.012, -0.02, 0.004][index % 4] ?? null,
    };
  });
  const monthly = kind === "monthly_carbon";
  return {
    time_zone: zone,
    scope,
    kpis: {
      total_kwh: total,
      carbon_kg: Math.round(total * CARBON_FACTOR),
      carbon_factor: CARBON_FACTOR,
      previous_period_kwh: Math.round(total / 1.042),
      previous_period_change: 0.042,
      previous_year_kwh: monthly ? Math.round(total / 0.969) : null,
      previous_year_change: monthly ? -0.031 : null,
    },
    daily,
    devices,
  };
}

function isEmail(text: string): boolean {
  return /^[^\s@]+@[^\s@.]+(\.[^\s@.]+)+$/.test(text) && text.length <= 254;
}

export class FakeReportServer {
  plan: "business" | "starter" = "business";
  /** Time zone of new schedules' next runs and reports. */
  zone = "Asia/Taipei";
  readonly schedules: ReportSchedule[] = [];
  readonly reports: ReportDetail[] = [];
  /** Names of the devices a report can be scoped to, by ID (a test adds the ones it uses). */
  readonly deviceNames = new Map<string, string>();
  /** The scope each report was made for; a report being made has no content to say it. */
  private readonly scopes = new Map<string, ReportScope>();
  readonly calls: FakeReportCall[] = [];
  readonly downloads: { reportId: string; format: DownloadFormat }[] = [];
  private failures = new Map<Endpoint, unknown[]>();
  private held = new Set<Endpoint>();
  private waiting: (() => void)[] = [];
  /** The newest report's creation time; each new one is a little later, so the order is stable. */
  private clock = Date.now();

  /** Fails the next call to `endpoint` with `error`. */
  failNext(endpoint: Endpoint, error: unknown): void {
    this.failures.set(endpoint, [...(this.failures.get(endpoint) ?? []), error]);
  }

  /** Keeps calls to these endpoints waiting until `release`. */
  hold(...endpoints: Endpoint[]): void {
    for (const endpoint of endpoints) this.held.add(endpoint);
  }

  release(): void {
    this.held.clear();
    for (const resume of this.waiting.splice(0)) resume();
  }

  private async enter(endpoint: Endpoint, args: unknown[]): Promise<void> {
    this.calls.push({ endpoint, args });
    if (this.held.has(endpoint)) await new Promise<void>((resolve) => this.waiting.push(resolve));
    if (this.plan === "starter") {
      throw apiError(403, "permission_denied", "entitlement.feature_not_in_plan");
    }
    const failure = this.failures.get(endpoint)?.shift();
    if (failure !== undefined) throw failure;
  }

  private notFound(): Error {
    return apiError(404, "not_found", "error.not_found");
  }

  /** Adds a schedule with the next run and the recipients it would have. */
  addSchedule(
    input: Partial<ScheduleInput> & { kind: ReportKind },
    extra: Partial<ReportSchedule> = {},
  ): ReportSchedule {
    const schedule: ReportSchedule = {
      id: fakeUuid(),
      kind: input.kind,
      scope: input.scope ?? { kind: "all" },
      send_on: input.send_on ?? 1,
      time: input.time ?? "08:00",
      time_zone: input.time_zone ?? this.zone,
      recipients: input.recipients ?? ["lin.mei@example.com"],
      enabled: true,
      next_run_at: null,
      last_run: null,
      bounced_recipients: [],
      ...extra,
    };
    schedule.next_run_at = extra.next_run_at !== undefined ? extra.next_run_at : this.nextRun(schedule);
    this.schedules.push(schedule);
    return schedule;
  }

  private nextRun(schedule: ReportSchedule): string | null {
    if (!schedule.enabled) return null;
    const next = nextOccurrence(new Date(), {
      kind: schedule.kind,
      sendOn: schedule.send_on,
      time: schedule.time,
      timeZone: schedule.time_zone,
    });
    return next ? zonedInstant(next.sendDate, schedule.time, schedule.time_zone).toISOString() : null;
  }

  /** Adds a report of a period; `ready` ones carry their dataset. */
  addReport(
    kind: ReportKind,
    period: { start: string; end: string },
    status: ReportSummary["status"] = "ready",
    extra: Partial<ReportSummary> & { scope?: ReportScope } = {},
  ): ReportDetail {
    this.clock += 1000;
    const { scope = { kind: "all" } as ReportScope, ...summary } = extra;
    const report: ReportDetail = {
      id: fakeUuid(),
      schedule_id: null,
      kind,
      period_start: period.start,
      period_end: period.end,
      status,
      generated_at: status === "generating" ? null : new Date(this.clock).toISOString(),
      failure: status === "failed" ? "query_timeout" : null,
      ...summary,
      content:
        status === "ready" ? makeContent(kind, period.start, period.end, scope, this.zone, this.deviceNames) : null,
    };
    this.scopes.set(report.id, scope);
    this.reports.push(report);
    return report;
  }

  /** Every report that is being made becomes ready. */
  finish(): void {
    for (const report of this.reports) {
      if (report.status !== "generating") continue;
      this.clock += 1000;
      report.status = "ready";
      report.generated_at = new Date(this.clock).toISOString();
      const scope = this.scopes.get(report.id) ?? { kind: "all" };
      report.content = makeContent(
        report.kind,
        report.period_start,
        report.period_end,
        scope,
        this.zone,
        this.deviceNames,
      );
    }
  }

  private summary(report: ReportDetail): ReportSummary {
    const { content: _content, ...summary } = report;
    return summary;
  }

  /** The period a schedule's report made right now covers. */
  private currentPeriod(schedule: ReportSchedule): { start: string; end: string } {
    const today = isoDate(Date.now());
    if (schedule.kind === "weekly_energy") {
      const monday = addDays(today, 1 - weekdayOf(today));
      return { start: addDays(monday, -7), end: addDays(monday, -1) };
    }
    const year = Number(today.slice(0, 4));
    const month = Number(today.slice(5, 7));
    return { start: isoDate(Date.UTC(year, month - 2, 1)), end: isoDate(Date.UTC(year, month - 1, 0)) };
  }

  private checked(input: ScheduleInput): void {
    if (input.recipients.length === 0) {
      throw apiError(422, "validation_failed", "error.validation_failed", [
        { field: "/recipients", message_key: "reports.form.error.recipients_empty" },
      ]);
    }
    const index = input.recipients.findIndex((email) => !isEmail(email));
    if (index >= 0) {
      throw apiError(422, "validation_failed", "error.validation_failed", [
        {
          field: `/recipients/${index}`,
          message_key: "reports.recipient_invalid",
          details: { email: input.recipients[index] ?? "" },
        },
      ]);
    }
  }

  api(): ReportApi {
    return {
      listSchedules: async (signal) => {
        await this.enter("listSchedules", [signal]);
        return this.schedules.map((schedule) => ({ ...schedule }));
      },
      getSchedule: async (scheduleId) => {
        await this.enter("getSchedule", [scheduleId]);
        const found = this.schedules.find((schedule) => schedule.id === scheduleId);
        if (!found) throw this.notFound();
        return { ...found };
      },
      createSchedule: async (input) => {
        await this.enter("createSchedule", [input]);
        this.checked(input);
        return { ...this.addSchedule(input) };
      },
      updateSchedule: async (scheduleId, input) => {
        await this.enter("updateSchedule", [scheduleId, input]);
        const found = this.schedules.find((schedule) => schedule.id === scheduleId);
        if (!found) throw this.notFound();
        this.checked(input);
        Object.assign(found, input, { bounced_recipients: [] });
        found.next_run_at = this.nextRun(found);
        return { ...found };
      },
      setScheduleEnabled: async (scheduleId, enabled) => {
        await this.enter("setScheduleEnabled", [scheduleId, enabled]);
        const found = this.schedules.find((schedule) => schedule.id === scheduleId);
        if (!found) throw this.notFound();
        found.enabled = enabled;
        found.next_run_at = this.nextRun(found);
        return { ...found };
      },
      deleteSchedule: async (scheduleId) => {
        await this.enter("deleteSchedule", [scheduleId]);
        const index = this.schedules.findIndex((schedule) => schedule.id === scheduleId);
        if (index < 0) throw this.notFound();
        this.schedules.splice(index, 1);
      },
      generateNow: async (scheduleId) => {
        await this.enter("generateNow", [scheduleId]);
        const found = this.schedules.find((schedule) => schedule.id === scheduleId);
        if (!found) throw this.notFound();
        const report = this.addReport(found.kind, this.currentPeriod(found), "generating", {
          schedule_id: found.id,
          scope: found.scope,
        });
        return this.summary(report);
      },
      listReports: async (query, signal) => {
        await this.enter("listReports", [query, signal]);
        const matching = this.reports
          .filter((report) => query.kind === undefined || report.kind === query.kind)
          .sort(
            (a, b) =>
              (a.period_end === b.period_end ? 0 : a.period_end < b.period_end ? 1 : -1) || b.id.localeCompare(a.id),
          );
        const offset = query.cursor ? Number(query.cursor) : 0;
        const limit = query.limit ?? 50;
        const page = matching.slice(offset, offset + limit).map((report) => this.summary(report));
        const next = offset + limit < matching.length ? String(offset + limit) : null;
        const list: ReportList = { reports: page, next_cursor: next };
        return list;
      },
      getReport: async (reportId, signal) => {
        await this.enter("getReport", [reportId, signal]);
        const found = this.reports.find((report) => report.id === reportId);
        if (!found) throw this.notFound();
        return { ...found };
      },
      retryReport: async (reportId) => {
        await this.enter("retryReport", [reportId]);
        const found = this.reports.find((report) => report.id === reportId);
        if (!found) throw this.notFound();
        found.status = "generating";
        found.failure = null;
        found.generated_at = null;
        return this.summary(found);
      },
      download: async (reportId, format) => {
        await this.enter("download", [reportId, format]);
        const found = this.reports.find((report) => report.id === reportId);
        if (!found || found.content === null) throw this.notFound();
        this.downloads.push({ reportId, format });
        if (format === "pdf") return new Blob(["Preview file: not a real PDF.\n"], { type: "application/pdf" });
        const rows = found.content.daily.map((day) => `${day.date},${day.kwh}`);
        return new Blob([`date,kwh\n${rows.join("\n")}\n`], { type: "text/csv" });
      },
    };
  }

  /** Seeds what docs/design/ms4-ms5/reports.html draws as the normal state. */
  seedNormal(): this {
    const now = Date.now();
    const today = isoDate(now);
    const monday = addDays(today, 1 - weekdayOf(today));
    const lastMonday = addDays(monday, -7);
    const firstOfMonth = `${today.slice(0, 8)}01`;
    const lastMonthEnd = addDays(firstOfMonth, -1);
    const lastMonthStart = `${lastMonthEnd.slice(0, 8)}01`;
    const weekly = this.addSchedule(
      { kind: "weekly_energy", recipients: ["lin.mei@example.com", "chen.wei@example.com", "yu.ting@example.com"] },
      {
        last_run: {
          period_start: addDays(lastMonday, -7),
          period_end: addDays(lastMonday, -1),
          at: new Date(now - 6 * DAY_MS).toISOString(),
          outcome: "sent",
        },
      },
    );
    const monthly = this.addSchedule(
      { kind: "monthly_carbon", send_on: 1, recipients: ["lin.mei@example.com", "plant-mgr@example.com"] },
      {
        last_run: {
          period_start: lastMonthStart,
          period_end: lastMonthEnd,
          at: new Date(now - 3 * DAY_MS).toISOString(),
          outcome: "sent",
        },
        bounced_recipients: ["plant-mgr@example.com"],
      },
    );
    this.addReport("weekly_energy", { start: addDays(lastMonday, -14), end: addDays(lastMonday, -8) }, "failed", {
      schedule_id: weekly.id,
    });
    this.addReport("weekly_energy", { start: addDays(lastMonday, -7), end: addDays(lastMonday, -1) }, "ready", {
      schedule_id: weekly.id,
    });
    this.addReport("monthly_carbon", { start: lastMonthStart, end: lastMonthEnd }, "ready", {
      schedule_id: monthly.id,
    });
    this.addReport("weekly_energy", { start: lastMonday, end: addDays(lastMonday, 6) }, "generating", {
      schedule_id: weekly.id,
    });
    return this;
  }
}

/**
 * The preview's states that the toolbar cannot reach (it only acts on HTTP fakes): a page that is
 * opened with `?fake-reports=starter`, `error` or `slow` shows the Starter wall, a failed load or a
 * slow answer.
 */
function previewMode(): string | null {
  if (typeof window === "undefined") return null;
  return new URLSearchParams(window.location.search).get("fake-reports");
}

export const preview: ProviderPreviewDeclaration = {
  name: "reports",
  provider: (seed) => {
    const server = new FakeReportServer();
    const mode = previewMode();
    if (seed === "normal") server.seedNormal();
    if (mode === "starter") server.plan = "starter";
    const api = server.api();
    // A report that is being made turns ready after a few seconds, so the list shows it change.
    const autoFinish = (): void => void setTimeout(() => server.finish(), 4000);
    const wrapped: ReportApi = {
      ...api,
      generateNow: async (id) => {
        const report = await api.generateNow(id);
        autoFinish();
        return report;
      },
      retryReport: async (id) => {
        const report = await api.retryReport(id);
        autoFinish();
        return report;
      },
    };
    if (seed === "normal") autoFinish();
    const slow = mode === "slow";
    const failing = mode === "error";
    const guarded = Object.fromEntries(
      Object.entries(wrapped).map(([name, call]) => [
        name,
        async (...args: unknown[]) => {
          if (slow) await new Promise((resolve) => setTimeout(resolve, 5000));
          if (failing) throw apiError(500, "internal", "error.internal");
          return (call as (...inner: unknown[]) => unknown)(...args);
        },
      ]),
    ) as unknown as ReportApi;
    const Provider: ComponentType<{ children: ReactNode }> = ({ children }) =>
      createElement(ReportApiProvider, { api: guarded, children });
    return Provider;
  },
};
