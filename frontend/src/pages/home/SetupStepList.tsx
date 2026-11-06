import { ArrowRightIcon, CheckIcon, CircleAlertIcon } from "lucide-react";
import type { Ref } from "react";
import { Link } from "react-router";

import type { SetupState, SetupStep, SetupUseId } from "../../api/pending/setup.ts";
import { Button, focusRing } from "../../components/ui/button.tsx";
import type { I18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { STEP_TARGETS, STEP_TEXT } from "./setup-steps.ts";

/** What the list needs from the checklist around it. */
export interface SetupStepListProps {
  state: SetupState;
  i18n: I18n;
  /** The ID of the `ol`, which the collapse button controls. */
  id: string;
  /** Steps to draw: all of them, or on a phone only the current one. */
  only?: SetupStep["id"] | undefined;
  tenantName: string;
  routeExists(path: string): boolean;
  /** A write is on its way: every button waits. */
  busy: boolean;
  /** The failure of the last write and the step or choice it was for. */
  failure: { step: SetupStep["id"]; message: string; kind: "skip" | "restore" | "choose" } | undefined;
  onSkip(step: SetupStep["id"]): void;
  onRestore(step: SetupStep["id"]): void;
  onChoose(use: SetupUseId): void;
  /** Where focus goes after a skip or a restore: the current step's main action. */
  actionRef: Ref<HTMLElement>;
}

const ICON = "grid size-6 shrink-0 place-items-center rounded-full text-xs font-semibold";

function StepIcon({ step, index, current }: { step: SetupStep; index: number; current: boolean }) {
  if (step.status === "done") {
    return (
      <span aria-hidden="true" className={cn(ICON, "bg-primary text-primary-foreground")}>
        <CheckIcon className="size-3.5" />
      </span>
    );
  }
  if (step.status === "skipped") {
    return (
      <span
        aria-hidden="true"
        className={cn(ICON, "border border-dashed border-muted-foreground text-muted-foreground")}
      >
        –
      </span>
    );
  }
  return (
    <span
      aria-hidden="true"
      className={cn(ICON, "border-2", current ? "border-primary text-primary" : "border-input text-muted-foreground")}
    >
      {index + 1}
    </span>
  );
}

/**
 * The checklist's steps, as docs/design/home-ai/ draws them: an ordered list, every state in a
 * shape and in words, only the current step open. The current step is the first one neither done
 * nor skipped.
 */
export function SetupStepList(props: SetupStepListProps) {
  const { state, i18n, id, only } = props;
  const current = state.steps.find((step) => step.status === "todo");
  const shown = state.steps
    .map((step, index) => ({ step, index }))
    .filter(({ step }) => only === undefined || step.id === only);
  return (
    <ol id={id} start={shown[0] ? shown[0].index + 1 : 1} className="grid gap-1">
      {shown.map(({ step, index }) => (
        <StepRow key={step.id} {...props} step={step} index={index} isCurrent={current?.id === step.id} i18n={i18n} />
      ))}
    </ol>
  );
}

function StepRow({
  step,
  index,
  isCurrent,
  state,
  i18n,
  tenantName,
  routeExists,
  busy,
  failure,
  onSkip,
  onRestore,
  onChoose,
  actionRef,
}: SetupStepListProps & { step: SetupStep; index: number; isCurrent: boolean }) {
  const { t, formatDateTime, formatNumber } = i18n;
  const text = STEP_TEXT[step.id];
  const title = t(text.title);
  const statusWord = t(
    step.status === "done"
      ? "setup.status.done"
      : step.status === "skipped"
        ? "setup.status.skipped"
        : isCurrent
          ? "setup.status.current"
          : "setup.status.todo",
  );
  const stepFailure = failure?.step === step.id ? failure : undefined;

  const target = STEP_TARGETS[step.id];
  const link = target !== null && routeExists(target) ? target : null;

  let description: string | null = null;
  if (isCurrent) {
    if (step.id === "bind_gateway") description = t("setup.step.bind_gateway.body", { tenant: tenantName });
    else if (step.id === "add_device") description = t("setup.step.add_device.body");
    else if (step.id === "first_data") {
      const count = state.steps.find((item) => item.id === "add_device")?.device_count;
      description =
        count === undefined
          ? t("setup.step.first_data.body")
          : t("setup.step.first_data.body_count", { count: formatNumber(count) });
    } else if (step.id === "first_use") description = t("setup.step.first_use.body");
    else {
      description =
        state.remaining_member_slots === null
          ? t("setup.step.invite_members.body")
          : t("setup.step.invite_members.body_slots", { count: formatNumber(state.remaining_member_slots) });
    }
  }

  return (
    <li
      aria-current={isCurrent ? "step" : undefined}
      className={cn(
        "grid grid-cols-[1.5rem_minmax(0,1fr)_auto] items-center gap-x-3 gap-y-2 rounded-md px-3 py-2 text-sm",
        isCurrent && "bg-brand-soft",
      )}
    >
      <StepIcon step={step} index={index} current={isCurrent} />
      <span className={cn("font-medium", step.status === "todo" && !isCurrent && "text-muted-foreground")}>
        {title}
        <span className="sr-only"> ({statusWord})</span>
      </span>
      <span className="flex flex-wrap items-center justify-end gap-x-2 text-xs text-muted-foreground">
        {step.status === "done" && step.completed_at ? (
          <span>
            {t("setup.step.completed_at", { time: formatDateTime(step.completed_at) })}
            {step.id === "add_device" && step.device_count !== undefined
              ? t("setup.step.device_count", { count: formatNumber(step.device_count) })
              : ""}
          </span>
        ) : null}
        {step.status === "skipped" ? (
          <>
            <span>{t("setup.step.skipped")}</span>
            <Button
              variant="link"
              size="sm"
              className="h-auto text-xs"
              disabled={busy}
              onClick={() => onRestore(step.id)}
              aria-label={t("setup.step.restore_named", { step: title })}
            >
              {t("setup.step.restore")}
            </Button>
          </>
        ) : null}
      </span>

      {stepFailure && stepFailure.kind === "restore" ? <StepError message={stepFailure.message} /> : null}

      {isCurrent ? (
        <div className="col-start-2 col-end-4 grid gap-3">
          {description ? <p className="text-foreground text-pretty">{description}</p> : null}
          {step.id === "first_data" ? (
            <p role="status" className="flex items-center gap-2 text-muted-foreground">
              <span aria-hidden="true" className="size-2 shrink-0 animate-pulse rounded-full bg-primary" />
              {state.last_data_check_at
                ? t("setup.step.first_data.waiting_checked", { time: formatDateTime(state.last_data_check_at) })
                : t("setup.step.first_data.waiting")}
            </p>
          ) : null}
          {step.id === "first_use" ? <UseCards state={state} i18n={i18n} busy={busy} onChoose={onChoose} /> : null}
          {stepFailure && stepFailure.kind !== "restore" ? <StepError message={stepFailure.message} /> : null}
          <div className="flex flex-wrap items-center gap-2">
            {link !== null && text.action !== null ? (
              <Button asChild size="sm">
                <Link to={link} ref={actionRef as Ref<HTMLAnchorElement>}>
                  {t("setup.step.next", { action: t(text.action) })}
                  <ArrowRightIcon aria-hidden="true" />
                </Link>
              </Button>
            ) : null}
            <Button
              variant="ghost"
              size="sm"
              disabled={busy}
              onClick={() => onSkip(step.id)}
              ref={link === null ? (actionRef as Ref<HTMLButtonElement>) : undefined}
              aria-label={t("setup.step.skip_named", { step: title })}
            >
              {t("setup.step.skip")}
            </Button>
          </div>
        </div>
      ) : null}
    </li>
  );
}

function StepError({ message }: { message: string }) {
  return (
    <p role="alert" className="col-start-2 col-end-4 flex items-start gap-2 text-sm text-destructive">
      <CircleAlertIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0" />
      <span>{message}</span>
    </p>
  );
}

function UseCards({
  state,
  i18n,
  busy,
  onChoose,
}: {
  state: SetupState;
  i18n: I18n;
  busy: boolean;
  onChoose(use: SetupUseId): void;
}) {
  const { t } = i18n;
  return (
    <ul className="grid gap-2 sm:grid-cols-2">
      {state.uses.map((use) => (
        <li key={use.id} className="grid">
          <button
            type="button"
            disabled={busy}
            onClick={() => onChoose(use.id)}
            className={cn(
              "grid content-start gap-1 rounded-lg border bg-card p-3 text-left text-card-foreground transition-colors hover:border-primary hover:bg-accent disabled:opacity-50",
              focusRing,
            )}
          >
            <span className="font-medium">{t(`setup.use.${use.id}.title`)}</span>
            <span className="text-xs text-muted-foreground">{t(`setup.use.${use.id}.body`)}</span>
            {use.required_plan !== null ? (
              <span className="text-xs text-muted-foreground">{t("setup.use.plan", { plan: use.required_plan })}</span>
            ) : null}
          </button>
        </li>
      ))}
    </ul>
  );
}
