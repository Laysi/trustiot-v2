import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { InfoIcon, XIcon } from "lucide-react";
import { type KeyboardEvent, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import { useDeviceApi } from "../../api/DeviceApiProvider.tsx";
import { classifyError } from "../../api/errors.ts";
import { useReportApi } from "../../api/pending/ReportApiProvider.tsx";
import type { ReportKind, ReportSchedule } from "../../api/pending/reports.ts";
import { ErrorSummary, type SummaryItem } from "../../components/ErrorSummary.tsx";
import { flashState } from "../../components/FlashNotice.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { useSession } from "../../session/SessionProvider.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { deviceListKey } from "../device/queries.ts";
import { type Problem, problemsOf, problemText } from "../form-problems.ts";
import { StatusPage } from "../StatusPage.tsx";
import { WriteDenied } from "../WriteDenied.tsx";
import { isNotInPlan, useReportRole } from "./access.ts";
import { isReportKind, KIND_DESCRIPTION, KIND_NAME } from "./kinds.ts";
import { FIRST_USE_DONE } from "./onboarding.ts";
import { PlanWall } from "./PlanWall.tsx";
import { REPORTS_PATH } from "./paths.ts";
import { reportListsKey, scheduleKey, schedulesKey } from "./queries.ts";
import { monthDay, nextOccurrence, weekdayOf } from "./schedule-time.ts";
import {
  baseOf,
  commitRecipient,
  newValues,
  POINTERS,
  type ScheduleFormValues,
  toInput,
  validate,
  valuesOf,
} from "./scheduleForm.ts";
import { WEEKDAYS, weekdayName } from "./weekday.ts";

/** Where each problem's input is named, for the summary's lines. */
const FIELD_LABELS: Readonly<Record<string, MessageId>> = {
  [POINTERS.recipients]: "reports.form.recipients",
  [POINTERS.scope]: "reports.form.scope",
  [POINTERS.time]: "reports.form.time",
};

const DAYS_OF_MONTH = Array.from({ length: 28 }, (_, index) => index + 1);

/**
 * The form that sets up a schedule (`/reports/schedules/new`) or changes one
 * (`/reports/schedules/:scheduleId/edit`), as docs/design/ms4-ms5/reports.html draws it. Opened
 * with `?preset=weekly&from=onboarding` it is the setup checklist's step "choose a first use":
 * filled in with every device, Monday 08:00 and the user as the only recipient, and saving it
 * goes back to the conversation home with the step done. Editors and admins only.
 */
export function ScheduleFormPage() {
  const { scheduleId } = useParams();
  const editing = scheduleId !== undefined;
  const { t, errorMessage } = useI18n();
  const api = useReportApi();
  const { canEdit, tenantId } = useReportRole();
  const [params] = useSearchParams();

  // The schedule list tells a plan without reports (Starter) before a form is shown for nothing.
  const schedules = useQuery({
    queryKey: schedulesKey(tenantId),
    queryFn: ({ signal }) => api.listSchedules(signal),
    enabled: canEdit,
  });
  const schedule = useQuery({
    queryKey: scheduleKey(tenantId, scheduleId ?? ""),
    queryFn: ({ signal }) => api.getSchedule(scheduleId ?? "", signal),
    enabled: canEdit && editing,
  });

  if (!canEdit) {
    return (
      <WriteDenied
        title={t("reports.form.denied_title")}
        body={t("reports.form.denied_body")}
        backTo={REPORTS_PATH}
        back={t("reports.not_found.back")}
      />
    );
  }
  if (isNotInPlan(schedules.error) || isNotInPlan(schedule.error)) {
    return (
      <div className="grid gap-4">
        <PageHeader title={t("reports.title")} />
        <PlanWall />
      </div>
    );
  }
  if (editing && schedule.isPending) {
    return (
      <p role="status" className="text-sm text-muted-foreground">
        {t("reports.form.loading")}
      </p>
    );
  }
  if (editing && schedule.data === undefined) {
    if (classifyError(schedule.error) === "not_found") {
      return (
        <StatusPage
          title={t("reports.not_found.title")}
          actions={
            <Button asChild variant="outline">
              <Link to={REPORTS_PATH}>{t("reports.not_found.back")}</Link>
            </Button>
          }
        >
          <p>{t("reports.not_found.body")}</p>
        </StatusPage>
      );
    }
    return (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("reports.form.load_failed", { reason: errorMessage(schedule.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void schedule.refetch()}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }
  const kindParam = params.get("kind");
  const preset = params.get("preset");
  const kind: ReportKind = isReportKind(kindParam) ? kindParam : "weekly_energy";
  const onboarding = !editing && preset === "weekly" && params.get("from") === "onboarding";
  return (
    <ScheduleForm
      key={scheduleId ?? "new"}
      schedule={schedule.data}
      kind={onboarding ? "weekly_energy" : kind}
      onboarding={onboarding}
    />
  );
}

function ScheduleForm({
  schedule,
  kind,
  onboarding,
}: {
  schedule: ReportSchedule | undefined;
  kind: ReportKind;
  onboarding: boolean;
}) {
  const { t, locale, errorMessage } = useI18n();
  const api = useReportApi();
  const devicesApi = useDeviceApi();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { timeZone } = usePreferences();
  const { tenantId } = useReportRole();
  const self = useSession().session?.user.email ?? null;
  const editing = schedule !== undefined;

  const [values, setValues] = useState<ScheduleFormValues>(() =>
    schedule ? valuesOf(schedule) : newValues(kind, self),
  );
  const [problems, setProblems] = useState<Problem[]>([]);
  const [attempt, setAttempt] = useState(0);
  const [refusal, setRefusal] = useState<unknown>(null);
  const inputs = useRef(new Map<string, HTMLElement | null>());
  const ref = (pointer: string) => (element: HTMLElement | null) => {
    inputs.current.set(pointer, element);
  };

  const devices = useQuery({
    queryKey: deviceListKey(tenantId),
    queryFn: ({ signal }) => devicesApi.listDevices(signal),
  });

  const save = useMutation({
    mutationFn: (input: ReturnType<typeof toInput>) =>
      schedule ? api.updateSchedule(schedule.id, input) : api.createSchedule(input),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: schedulesKey(tenantId) }),
        queryClient.invalidateQueries({ queryKey: reportListsKey(tenantId) }),
        ...(schedule ? [queryClient.invalidateQueries({ queryKey: scheduleKey(tenantId, schedule.id) })] : []),
      ]);
      if (onboarding) {
        void navigate("/", { state: FIRST_USE_DONE });
      } else {
        void navigate(REPORTS_PATH, {
          state: flashState(editing ? "reports.flash.updated" : "reports.flash.created"),
        });
      }
    },
    onError: (error) => {
      const { problems: found, rest } = problemsOf(error);
      setProblems(found);
      setRefusal(rest);
      if (found.length > 0) setAttempt((count) => count + 1);
    },
  });

  const change = (patch: Partial<ScheduleFormValues>, pointer?: string) => {
    setValues((current) => ({ ...current, ...patch }));
    if (pointer !== undefined)
      setProblems((current) => current.filter((problem) => baseOf(problem.pointer) !== pointer));
  };

  const submit = () => {
    const committed = commitRecipient(values);
    const found = [...validate(committed.values), ...(committed.problem ? [committed.problem] : [])];
    setValues(committed.values);
    setRefusal(null);
    if (found.length > 0) {
      setProblems(found);
      setAttempt((count) => count + 1);
      return;
    }
    setProblems([]);
    save.mutate(toInput(committed.values, schedule?.time_zone ?? timeZone));
  };

  const errorOf = (base: string): string | undefined => {
    const problem = problems.find((candidate) => baseOf(candidate.pointer) === base);
    return problem ? problemText(locale, problem) : undefined;
  };
  const summary: SummaryItem[] = problems.map((problem, index) => {
    const base = baseOf(problem.pointer);
    const label = FIELD_LABELS[base];
    const text = problemText(locale, problem);
    return {
      key: `${problem.pointer}-${index}`,
      text: label ? t("form.problem_line", { field: t(label), message: text }) : text,
      focus: () => inputs.current.get(base)?.focus(),
    };
  });

  const zone = schedule?.time_zone ?? timeZone;
  const occurrence = useMemo(
    () => nextOccurrence(new Date(), { kind: values.kind, sendOn: values.sendOn, time: values.time, timeZone: zone }),
    [values.kind, values.sendOn, values.time, zone],
  );

  const title = onboarding
    ? t("reports.form.title_weekly")
    : editing
      ? t("reports.form.title_edit")
      : t("reports.form.title_new");
  const weekly = values.kind === "weekly_energy";
  const deviceList = devices.data ?? [];
  const recipientError = errorOf(POINTERS.recipients);
  const submitting = save.isPending;

  const onRecipientKey = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter" || event.key === ",") {
      event.preventDefault();
      const committed = commitRecipient(values);
      setValues(committed.values);
      setProblems((current) => [
        ...current.filter((problem) => baseOf(problem.pointer) !== POINTERS.recipients),
        ...(committed.problem ? [committed.problem] : []),
      ]);
    } else if (event.key === "Backspace" && values.recipientDraft === "" && values.recipients.length > 0) {
      change({ recipients: values.recipients.slice(0, -1) }, POINTERS.recipients);
    }
  };

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      {onboarding ? (
        <Alert role="status" className="max-w-2xl grid-cols-[1rem_1fr]">
          <InfoIcon aria-hidden="true" />
          <AlertDescription>
            <p>{t("reports.form.onboarding")}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      <PageHeader title={title} crumbs={onboarding ? undefined : [{ label: t("reports.title"), to: REPORTS_PATH }]} />

      <form
        noValidate
        aria-label={title}
        className="grid max-w-2xl gap-5 rounded-xl border bg-card p-5 text-card-foreground shadow-sm"
        onSubmit={(event) => {
          event.preventDefault();
          if (!submitting) submit();
        }}
      >
        <ErrorSummary items={summary} attempt={attempt} />
        {refusal !== null ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(refusal)}</p>
            </AlertDescription>
          </Alert>
        ) : null}

        <fieldset disabled={editing} className="grid gap-2">
          <legend className="mb-1 text-sm leading-none font-medium">{t("reports.form.report")}</legend>
          <div className="grid gap-3 sm:grid-cols-2">
            {(["weekly_energy", "monthly_carbon"] as const).map((option) => (
              <label
                key={option}
                className={cn(
                  "flex cursor-pointer items-start gap-3 rounded-lg border p-3 text-sm has-[:checked]:border-primary has-[:checked]:bg-brand-soft has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-ring",
                  editing && "opacity-70",
                )}
              >
                <input
                  type="radio"
                  name="kind"
                  value={option}
                  checked={values.kind === option}
                  onChange={() => change({ kind: option, sendOn: 1 })}
                  className="mt-0.5 size-4 accent-primary"
                />
                <span className="grid gap-0.5">
                  <span className="font-medium">{t(KIND_NAME[option])}</span>
                  <span className="text-muted-foreground">{t(KIND_DESCRIPTION[option])}</span>
                </span>
              </label>
            ))}
          </div>
        </fieldset>

        <FormField label={t("reports.form.scope")} error={errorOf(POINTERS.scope)}>
          {(control) => (
            <>
              <NativeSelect
                {...control}
                ref={ref(POINTERS.scope)}
                value={values.scopeMode}
                onChange={(event) =>
                  change({ scopeMode: event.target.value === "devices" ? "devices" : "all" }, POINTERS.scope)
                }
              >
                <option value="all">{t("reports.form.scope_all", { count: deviceList.length })}</option>
                <option value="devices">{t("reports.form.scope_choose")}</option>
              </NativeSelect>
              {values.scopeMode === "devices" ? (
                <fieldset className="grid gap-1.5 rounded-lg border p-3">
                  <legend className="px-1 text-sm text-muted-foreground">{t("reports.form.scope_legend")}</legend>
                  {devices.isPending ? <Spinner label={t("reports.form.devices_loading")} /> : null}
                  {devices.isError ? (
                    <p role="alert" className="text-sm text-destructive">
                      {t("reports.form.devices_failed", { reason: errorMessage(devices.error) })}
                    </p>
                  ) : null}
                  {deviceList.map((device) => (
                    <label key={device.device_id} className="flex items-center gap-2 text-sm">
                      <input
                        type="checkbox"
                        checked={values.deviceIds.includes(device.device_id)}
                        onChange={(event) =>
                          change(
                            {
                              deviceIds: event.target.checked
                                ? [...values.deviceIds, device.device_id]
                                : values.deviceIds.filter((id) => id !== device.device_id),
                            },
                            POINTERS.scope,
                          )
                        }
                        className="size-4 accent-primary"
                      />
                      {device.name}
                    </label>
                  ))}
                </fieldset>
              ) : null}
            </>
          )}
        </FormField>

        <div className="grid gap-4 sm:grid-cols-2">
          <FormField label={t(weekly ? "reports.form.send_on_weekly" : "reports.form.send_on_monthly")}>
            {(control) => (
              <NativeSelect
                {...control}
                value={String(values.sendOn)}
                onChange={(event) => change({ sendOn: Number(event.target.value) })}
              >
                {weekly
                  ? WEEKDAYS.map((day) => (
                      <option key={day} value={day}>
                        {t("reports.form.every_weekday", { weekday: weekdayName(day, locale) })}
                      </option>
                    ))
                  : DAYS_OF_MONTH.map((day) => (
                      <option key={day} value={day}>
                        {t("reports.form.day_of_month", { day: String(day) })}
                      </option>
                    ))}
              </NativeSelect>
            )}
          </FormField>
          <FormField
            label={t("reports.form.time")}
            description={t("reports.form.time_zone_hint", { zone })}
            error={errorOf(POINTERS.time)}
          >
            {(control) => (
              <Input
                {...control}
                ref={ref(POINTERS.time)}
                type="time"
                value={values.time}
                onChange={(event) => change({ time: event.target.value }, POINTERS.time)}
              />
            )}
          </FormField>
        </div>

        <FormField
          label={t("reports.form.recipients")}
          description={t("reports.form.recipients_hint")}
          error={recipientError}
        >
          {(control) => (
            <div
              className={cn(
                "flex min-h-9 flex-wrap items-center gap-1.5 rounded-md border bg-background px-2 py-1.5 shadow-xs focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-ring",
                recipientError ? "border-destructive" : "border-input",
              )}
            >
              {values.recipients.map((email) => (
                <span
                  key={email}
                  className="inline-flex max-w-full items-center gap-1 rounded-full border bg-muted py-0.5 pr-1 pl-2.5 text-sm"
                >
                  <span className="truncate">
                    {email === self ? t("reports.form.recipient_you", { email }) : email}
                  </span>
                  <button
                    type="button"
                    aria-label={t("reports.form.remove_recipient", { email })}
                    className={cn("rounded-full p-0.5 hover:bg-accent", focusRing)}
                    onClick={() =>
                      change({ recipients: values.recipients.filter((item) => item !== email) }, POINTERS.recipients)
                    }
                  >
                    <XIcon aria-hidden="true" className="size-3.5" />
                  </button>
                </span>
              ))}
              <input
                {...control}
                ref={ref(POINTERS.recipients)}
                type="email"
                inputMode="email"
                autoComplete="off"
                value={values.recipientDraft}
                placeholder={t("reports.form.recipients_placeholder")}
                className="min-w-40 flex-1 bg-transparent px-1 py-0.5 text-sm outline-none placeholder:text-muted-foreground"
                onChange={(event) => change({ recipientDraft: event.target.value }, POINTERS.recipients)}
                onKeyDown={onRecipientKey}
              />
            </div>
          )}
        </FormField>

        {occurrence ? (
          <p
            role="status"
            className="flex items-start gap-2 rounded-lg bg-brand-soft px-3 py-2 text-sm text-foreground"
          >
            <InfoIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-primary" />
            <span>
              {t("reports.form.preview", {
                date: monthDay(occurrence.sendDate),
                weekday: weekdayName(weekdayOf(occurrence.sendDate), locale, "short"),
                time: values.time,
                from: monthDay(occurrence.periodStart),
                to: monthDay(occurrence.periodEnd),
              })}
            </span>
          </p>
        ) : null}

        <div className="flex flex-wrap items-center justify-end gap-2">
          <Button asChild variant="outline">
            <Link to={onboarding ? "/" : REPORTS_PATH}>
              {t(onboarding ? "reports.form.back_to_conversation" : "reports.cancel")}
            </Link>
          </Button>
          <Button type="submit" disabled={submitting}>
            {submitting ? <Spinner /> : null}
            {submitting
              ? t("reports.form.saving")
              : t(
                  onboarding
                    ? "reports.form.save_onboarding"
                    : editing
                      ? "reports.form.save_edit"
                      : "reports.form.save",
                )}
          </Button>
        </div>
      </form>
    </div>
  );
}
