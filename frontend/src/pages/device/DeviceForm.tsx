import { type ReactNode, useRef } from "react";
import { Link } from "react-router";

import type { BoundGateway, PlanLimits } from "../../api/assets.ts";
import type { DeviceModelSummary } from "../../api/devices.ts";
import { modelsNewPath } from "../../app/paths.ts";
import { ErrorSummary, type SummaryItem } from "../../components/ErrorSummary.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { type Problem, problemText } from "../form-problems.ts";
import { type DeviceFormValues, POLL_CHOICES, unitRange } from "./deviceForm.ts";
import { useDeviceFormat } from "./format.ts";

/** Where each input's name comes from, for the summary's lines. */
const FIELD_LABELS: Readonly<Record<string, MessageId>> = {
  "/name": "device_form.name",
  "/hardware_id": "device_form.gateway",
  "/model_id": "device_form.model",
  "/transport": "device_form.transport",
  "/host": "device_form.host",
  "/port": "device_form.port",
  "/unit_id": "device_form.unit_id",
  "/poll_interval_ms": "device_form.poll",
};

/** A list the form fills a select from: loading, failed, or here. */
export interface Choices<T> {
  status: "pending" | "error" | "success";
  items: readonly T[];
  retry(): void;
}

/**
 * The device form (`/devices/new`, `/devices/:id/settings`), one page of three cards as
 * docs/design/tenant-admin/ draws it: basic data (name, gateway, model: fixed once the device
 * exists, so shown as text when editing), connection, and sampling. It shows the problems it is
 * given at their inputs, and in a summary that takes the focus after a failed submit.
 */
export function DeviceForm(props: {
  mode: "create" | "edit";
  values: DeviceFormValues;
  onChange(patch: Partial<DeviceFormValues>, pointer: string): void;
  problems: readonly Problem[];
  /** The number of submits that left problems: each moves the focus to the summary. */
  attempt: number;
  submitting: boolean;
  /** Every input is fixed (the device's gateway was unbound, so it can only be deleted). */
  locked?: boolean;
  gateways: Choices<BoundGateway>;
  models: Choices<DeviceModelSummary>;
  limits: PlanLimits | undefined;
  onSubmit(): void;
  cancelTo: string;
  submitLabel: string;
  /** Between the cards and the buttons' left side: the delete button of the edit page. */
  extraActions?: ReactNode;
  /** Above the form: refusals that are not about an input, conflicts. */
  notices?: ReactNode;
}) {
  const { t, locale } = useI18n();
  const format = useDeviceFormat();
  const { values, problems, locked = false } = props;
  const inputs = useRef(new Map<string, HTMLElement | null>());
  const ref = (pointer: string) => (element: HTMLElement | null) => {
    inputs.current.set(pointer, element);
  };
  const errorOf = (pointer: string) => {
    const problem = problems.find((candidate) => candidate.pointer === pointer);
    return problem ? problemText(locale, problem) : undefined;
  };
  const summary: SummaryItem[] = problems.map((problem, index) => {
    const label = FIELD_LABELS[problem.pointer];
    const text = problemText(locale, problem);
    return {
      key: `${problem.pointer}-${index}`,
      text: label ? t("form.problem_line", { field: t(label), message: text }) : text,
      focus: () => inputs.current.get(problem.pointer)?.focus(),
    };
  });
  const change = (pointer: string, patch: Partial<DeviceFormValues>) => props.onChange(patch, pointer);

  const range = unitRange(values.transport);
  const minimum = props.limits?.minPollIntervalMs;
  // A saved interval outside the list (set before, or by the API) stays selectable, marked as current.
  const current = Number(values.pollMs);
  const choices =
    POLL_CHOICES.includes(current) || !Number.isFinite(current)
      ? POLL_CHOICES
      : [...POLL_CHOICES, current].sort((a, b) => a - b);
  const pollLabel = (ms: number) => {
    const duration = format.duration(ms);
    if (!POLL_CHOICES.includes(ms)) return t("device_form.poll_current", { duration });
    return minimum !== undefined && ms < minimum ? t("device_form.poll_needs_plan", { duration }) : duration;
  };

  const model = props.models.items.find((candidate) => candidate.model_id === values.modelId);
  const gateway = props.gateways.items.find((candidate) => candidate.hardware_id === values.hardwareId);
  const presenceLabel = (presence: BoundGateway["presence"]) => t(`device_form.presence.${presence}`);

  return (
    <form
      noValidate
      className="grid gap-6"
      onSubmit={(event) => {
        event.preventDefault();
        if (!locked && !props.submitting) props.onSubmit();
      }}
    >
      {props.notices}
      <ErrorSummary items={summary} attempt={props.attempt} />

      <Card title={t("device_form.basics")}>
        <div className="grid gap-4 sm:grid-cols-3">
          {props.mode === "create" ? (
            <>
              <FormField
                label={t("device_form.name")}
                description={t("device_form.name_hint")}
                error={errorOf("/name")}
              >
                {(control) => (
                  <Input
                    {...control}
                    ref={ref("/name")}
                    value={values.name}
                    autoComplete="off"
                    spellCheck={false}
                    onChange={(event) => change("/name", { name: event.target.value })}
                  />
                )}
              </FormField>
              <FormField
                label={t("device_form.gateway")}
                description={
                  props.gateways.status === "error" ? (
                    <GatewaysFailed retry={props.gateways.retry} />
                  ) : props.gateways.status === "success" && props.gateways.items.length === 0 ? (
                    t("device_form.no_gateways")
                  ) : (
                    t("device_form.gateway_hint")
                  )
                }
                error={errorOf("/hardware_id")}
              >
                {(control) => (
                  <NativeSelect
                    {...control}
                    ref={ref("/hardware_id")}
                    value={values.hardwareId}
                    disabled={props.gateways.status !== "success"}
                    onChange={(event) => change("/hardware_id", { hardwareId: event.target.value })}
                  >
                    <option value="">{t("device_form.choose_gateway_option")}</option>
                    {props.gateways.items.map((item) => (
                      <option key={item.hardware_id} value={item.hardware_id}>
                        {t("device_form.gateway_option", {
                          gateway: item.hardware_id,
                          presence: presenceLabel(item.presence),
                        })}
                      </option>
                    ))}
                  </NativeSelect>
                )}
              </FormField>
              <FormField
                label={t("device_form.model")}
                description={
                  props.models.status === "error" ? (
                    <ModelsFailed retry={props.models.retry} />
                  ) : (
                    <>
                      {t("device_form.model_hint")}{" "}
                      <Link
                        to={modelsNewPath()}
                        className={cn("rounded-sm text-primary underline underline-offset-4", focusRing)}
                      >
                        {t("device_form.create_model")}
                      </Link>
                    </>
                  )
                }
                error={errorOf("/model_id")}
              >
                {(control) => (
                  <NativeSelect
                    {...control}
                    ref={ref("/model_id")}
                    value={values.modelId}
                    disabled={props.models.status !== "success"}
                    onChange={(event) => change("/model_id", { modelId: event.target.value })}
                  >
                    <option value="">{t("device_form.choose_model_option")}</option>
                    {props.models.items.map((item) => (
                      <option key={item.model_id} value={item.model_id}>
                        {t("device_form.model_option", { model: item.name, count: item.point_count })}
                      </option>
                    ))}
                  </NativeSelect>
                )}
              </FormField>
            </>
          ) : (
            <>
              <Fixed label={t("device_form.name")} value={values.name} />
              <Fixed label={t("device_form.gateway")} value={values.hardwareId} />
              <Fixed label={t("device_form.model")} value={model?.name ?? "—"} />
            </>
          )}
        </div>
        {props.mode === "create" && gateway === undefined && values.hardwareId !== "" ? (
          <p className="text-sm text-muted-foreground">
            {t("device_form.gateway_unlisted", { gateway: values.hardwareId })}
          </p>
        ) : null}
      </Card>

      <Card title={t("device_form.connection")} description={t("device_form.connection_hint")}>
        <fieldset disabled={locked} className="grid gap-2">
          <legend className="mb-2 text-sm font-medium">{t("device_form.transport")}</legend>
          <div className="grid gap-3 sm:grid-cols-2">
            {(["modbus_tcp", "rtu_over_tcp"] as const).map((transport, index) => (
              <label
                key={transport}
                className={cn(
                  "grid cursor-pointer gap-1 rounded-lg border p-3 has-[:checked]:border-primary has-[:checked]:bg-accent has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-offset-2 has-[:focus-visible]:outline-ring",
                  locked && "cursor-default opacity-70",
                )}
              >
                <span className="flex items-center gap-2 text-sm font-medium">
                  <input
                    type="radio"
                    name="transport"
                    value={transport}
                    checked={values.transport === transport}
                    ref={index === 0 ? ref("/transport") : undefined}
                    className="size-4 accent-primary"
                    onChange={() => change("/transport", { transport })}
                  />
                  {t(transport === "modbus_tcp" ? "device_form.transport.tcp" : "device_form.transport.rtu")}
                </span>
                <span className="text-sm text-muted-foreground">
                  {t(transport === "modbus_tcp" ? "device_form.transport.tcp_hint" : "device_form.transport.rtu_hint")}
                </span>
              </label>
            ))}
          </div>
          {errorOf("/transport") ? (
            <p className="text-sm font-medium text-destructive">{errorOf("/transport")}</p>
          ) : null}
        </fieldset>
        <div className="grid gap-4 sm:grid-cols-3">
          <FormField label={t("device_form.host")} description={t("device_form.host_hint")} error={errorOf("/host")}>
            {(control) => (
              <Input
                {...control}
                ref={ref("/host")}
                value={values.host}
                disabled={locked}
                autoComplete="off"
                spellCheck={false}
                onChange={(event) => change("/host", { host: event.target.value })}
              />
            )}
          </FormField>
          <FormField label={t("device_form.port")} description={t("device_form.port_hint")} error={errorOf("/port")}>
            {(control) => (
              <Input
                {...control}
                ref={ref("/port")}
                inputMode="numeric"
                value={values.port}
                disabled={locked}
                onChange={(event) => change("/port", { port: event.target.value })}
              />
            )}
          </FormField>
          <FormField
            label={t("device_form.unit_id")}
            description={t("device_form.unit_hint", range)}
            error={errorOf("/unit_id")}
          >
            {(control) => (
              <Input
                {...control}
                ref={ref("/unit_id")}
                inputMode="numeric"
                value={values.unitId}
                disabled={locked}
                onChange={(event) => change("/unit_id", { unitId: event.target.value })}
              />
            )}
          </FormField>
        </div>
      </Card>

      <Card title={t("device_form.sampling")}>
        <FormField
          label={t("device_form.poll")}
          description={
            props.limits
              ? t("device_form.poll_hint", {
                  plan: t(`device_form.plan.${props.limits.plan}`),
                  duration: format.duration(props.limits.minPollIntervalMs),
                })
              : t("device_form.poll_hint_unknown")
          }
          error={errorOf("/poll_interval_ms")}
          className="sm:max-w-xs"
        >
          {(control) => (
            <NativeSelect
              {...control}
              ref={ref("/poll_interval_ms")}
              value={values.pollMs}
              disabled={locked}
              onChange={(event) => change("/poll_interval_ms", { pollMs: event.target.value })}
            >
              {choices.map((ms) => (
                <option key={ms} value={String(ms)} disabled={minimum !== undefined && ms < minimum && ms !== current}>
                  {pollLabel(ms)}
                </option>
              ))}
            </NativeSelect>
          )}
        </FormField>
      </Card>

      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>{props.extraActions}</div>
        <div className="flex flex-wrap items-center gap-2">
          <Button variant="outline" asChild>
            <Link to={props.cancelTo}>{t("device_form.cancel")}</Link>
          </Button>
          {locked ? null : (
            <Button type="submit" disabled={props.submitting}>
              {props.submitting ? <Spinner /> : null}
              {props.submitting ? t("device_form.saving") : props.submitLabel}
            </Button>
          )}
        </div>
      </div>
    </form>
  );
}

function Card({ title, description, children }: { title: string; description?: string; children: ReactNode }) {
  return (
    <section className="grid gap-4 rounded-lg border bg-card px-4 py-5 text-card-foreground sm:px-6">
      <div className="grid gap-1">
        <h2 className="font-semibold">{title}</h2>
        {description ? <p className="text-sm text-muted-foreground">{description}</p> : null}
      </div>
      {children}
    </section>
  );
}

/** A setting that cannot change once the device exists, shown as text. */
function Fixed({ label, value }: { label: string; value: string }) {
  const { t } = useI18n();
  return (
    <div className="grid content-start gap-1">
      <p className="text-sm font-medium">{label}</p>
      <p className="font-mono text-sm wrap-anywhere">{value}</p>
      <p className="text-sm text-muted-foreground">{t("device_form.fixed_hint")}</p>
    </div>
  );
}

function GatewaysFailed({ retry }: { retry(): void }) {
  const { t } = useI18n();
  return (
    <>
      {t("device_form.gateways_failed")}{" "}
      <button
        type="button"
        className={cn("rounded-sm text-primary underline underline-offset-4", focusRing)}
        onClick={retry}
      >
        {t("page.action.retry")}
      </button>
    </>
  );
}

function ModelsFailed({ retry }: { retry(): void }) {
  const { t } = useI18n();
  return (
    <>
      {t("device_form.models_failed")}{" "}
      <button
        type="button"
        className={cn("rounded-sm text-primary underline underline-offset-4", focusRing)}
        onClick={retry}
      >
        {t("page.action.retry")}
      </button>
    </>
  );
}
