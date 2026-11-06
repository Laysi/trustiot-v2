import { CircleCheckIcon, TriangleAlertIcon } from "lucide-react";
import { type FormEvent, useId, useRef, useState } from "react";
import { Link } from "react-router";

import { usePlatformApi } from "../../api/pending/PlatformApiProvider.tsx";
import type { EnterpriseOverrides, Plan } from "../../api/pending/platform.ts";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "../../components/ui/card.tsx";
import { FormField } from "../../components/ui/form-field.tsx";
import { Input } from "../../components/ui/input.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { PLAN_LABELS } from "./format.ts";
import { PLATFORM_TENANTS_PATH, platformTenantPath } from "./paths.ts";
import { outcomeUnknown } from "./ui/ActionDialog.tsx";
import { AuditNote } from "./ui/audit-note.tsx";

type CreatablePlan = Extract<Plan, "starter" | "business" | "enterprise">;
const PLANS: readonly {
  plan: CreatablePlan;
  hint:
    | "platform.create.plan_hint.starter"
    | "platform.create.plan_hint.business"
    | "platform.create.plan_hint.enterprise";
}[] = [
  { plan: "starter", hint: "platform.create.plan_hint.starter" },
  { plan: "business", hint: "platform.create.plan_hint.business" },
  { plan: "enterprise", hint: "platform.create.plan_hint.enterprise" },
];

const NAME_MAX = 100;
const EMAIL = /^[^@\s]+@[^@\s]+\.[^@\s]+$/;

/** A whole number of 0 or more, or "unlimited" for the word, `undefined` for blank, `null` for anything else. */
function parseLimit(text: string): number | "unlimited" | undefined | null {
  const value = text.trim().toLowerCase();
  if (value === "") return undefined;
  if (value === "unlimited") return "unlimited";
  return /^\d{1,9}$/.test(value) ? Number(value) : null;
}

function parseUsd(text: string): number | undefined | null {
  const value = text.trim();
  if (value === "") return undefined;
  return /^\d{1,7}(\.\d{1,2})?$/.test(value) ? Number(value) : null;
}

interface Created {
  id: string;
  name: string;
}

/** What went wrong after the tenant exists: it must not be created a second time. */
type Incomplete = { created: Created; step: "plan" | "invite"; error: unknown };

/**
 * Creates a tenant (`/platform/tenants/new`, docs/design/platform-admin/tenant-create.html): name,
 * plan (an Enterprise contract may override the limits) and the first administrator's email, in
 * two steps on one page: the form, then a review that says what will be logged. Creating a tenant,
 * setting its plan and inviting the first admin are three calls; the tenant call is not idempotent,
 * so once the tenant exists the button never creates it again: a failure of a later step is shown
 * as "created, but ..." with a way to the tenant's page (design.md, 「建立租戶」).
 */
export function TenantCreatePage() {
  const { t, errorMessage, fieldError } = useI18n();
  const api = usePlatformApi();
  const [step, setStep] = useState<"form" | "review">("form");
  const [name, setName] = useState("");
  const [plan, setPlan] = useState<CreatablePlan>("starter");
  const [gateways, setGateways] = useState("");
  const [members, setMembers] = useState("");
  const [alerts, setAlerts] = useState("");
  const [ai, setAi] = useState("");
  const [email, setEmail] = useState("");
  const [checked, setChecked] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<unknown>(null);
  // The creation may have gone through without an answer: it is not sent again until the person says
  // they looked at the tenant list.
  const [unsure, setUnsure] = useState(false);
  const [reconciled, setReconciled] = useState(false);
  // Same for a plan change or an invitation whose answer was lost: the tenant's log is looked at first.
  const [partialChecked, setPartialChecked] = useState(false);
  const [partial, setPartial] = useState<Incomplete | null>(null);
  const [done, setDone] = useState<Created | null>(null);
  const created = useRef<Created | null>(null);
  const planDone = useRef(false);
  const planId = useId();

  const trimmed = name.trim();
  const limits = {
    maxGateways: parseLimit(gateways),
    maxMembers: parseLimit(members),
    maxAlertRules: parseLimit(alerts),
    aiMonthlyUsd: parseUsd(ai),
  };
  const overridesInvalid = plan === "enterprise" && Object.values(limits).some((value) => value === null);
  const nameError =
    checked && trimmed === ""
      ? t("platform.create.name_required")
      : trimmed.length > NAME_MAX
        ? t("tenant.name_invalid", { max: NAME_MAX })
        : undefined;
  const emailError = checked && !EMAIL.test(email.trim()) ? t("member.email_invalid") : undefined;
  const formValid = trimmed !== "" && trimmed.length <= NAME_MAX && EMAIL.test(email.trim()) && !overridesInvalid;

  const overrides = (): EnterpriseOverrides => {
    if (plan !== "enterprise") return {};
    const result: EnterpriseOverrides = {};
    if (typeof limits.maxGateways !== "undefined" && limits.maxGateways !== null)
      result.maxGateways = limits.maxGateways;
    if (typeof limits.maxMembers !== "undefined" && limits.maxMembers !== null) result.maxMembers = limits.maxMembers;
    if (typeof limits.maxAlertRules !== "undefined" && limits.maxAlertRules !== null)
      result.maxAlertRules = limits.maxAlertRules;
    if (typeof limits.aiMonthlyUsd !== "undefined" && limits.aiMonthlyUsd !== null)
      result.aiMonthlyUsd = limits.aiMonthlyUsd;
    return result;
  };

  const review = (event: FormEvent) => {
    event.preventDefault();
    setChecked(true);
    if (formValid) setStep("review");
  };

  const submit = async () => {
    if (submitting || (unsure && !reconciled)) return;
    setUnsure(false);
    setSubmitting(true);
    setError(null);
    try {
      if (created.current === null) {
        // Not idempotent: once it answered, this is never called again from this page.
        const tenant = await api.createTenant({ name: trimmed });
        created.current = { id: tenant.id, name: tenant.name };
      }
      const tenant = created.current;
      if (plan !== "starter" && !planDone.current) {
        try {
          await api.setTenantPlan(tenant.id, { plan, overrides: overrides() });
          planDone.current = true;
        } catch (cause) {
          setPartialChecked(false);
          setPartial({ created: tenant, step: "plan", error: cause });
          return;
        }
      }
      try {
        await api.inviteFirstAdmin(tenant.id, email.trim());
      } catch (cause) {
        setPartialChecked(false);
        setPartial({ created: tenant, step: "invite", error: cause });
        return;
      }
      setDone(tenant);
    } catch (cause) {
      setError(cause);
      if (created.current === null && outcomeUnknown(cause)) {
        setUnsure(true);
        setReconciled(false);
      }
    } finally {
      setSubmitting(false);
    }
  };

  const crumbs = [{ label: t("platform.tenants.title"), to: PLATFORM_TENANTS_PATH }];
  const title = t("platform.create.title");

  if (done) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={title} crumbs={crumbs} />
        <Alert role="status" className="grid-cols-[1rem_1fr]">
          <CircleCheckIcon aria-hidden="true" />
          <AlertTitle>{t("platform.create.done_title", { name: done.name })}</AlertTitle>
          <AlertDescription>
            <p>{t("platform.create.done_body", { email: email.trim() })}</p>
            <Button asChild variant="outline" size="sm">
              <Link to={platformTenantPath(done.id)}>{t("platform.create.open_tenant")}</Link>
            </Button>
          </AlertDescription>
        </Alert>
      </div>
    );
  }

  if (partial) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={title} crumbs={crumbs} />
        <Alert className="grid-cols-[1rem_1fr] border-warning/50 bg-warning-soft text-foreground">
          <TriangleAlertIcon aria-hidden="true" />
          <AlertTitle>
            {t(partial.step === "invite" ? "platform.create.partial_invite" : "platform.create.partial_plan", {
              name: partial.created.name,
            })}
          </AlertTitle>
          <AlertDescription className="text-foreground">
            <p>{errorMessage(partial.error)}</p>
            <p>{t("platform.create.partial_body")}</p>
            {outcomeUnknown(partial.error) ? (
              <div className="grid gap-2 rounded-md border bg-background px-3 py-2 text-sm">
                <a
                  className="w-fit underline underline-offset-4"
                  href={platformTenantPath(partial.created.id, "audit")}
                  target="_blank"
                  rel="noreferrer"
                >
                  {t("platform.create.open_log")}
                </a>
                <label className="flex items-start gap-2">
                  <input
                    type="checkbox"
                    className="mt-0.5 size-4 accent-primary"
                    checked={partialChecked}
                    onChange={(event) => setPartialChecked(event.target.checked)}
                  />
                  <span>
                    {t("platform.create.partial_checked", {
                      step: t(partial.step === "invite" ? "platform.create.step_invite" : "platform.create.step_plan"),
                    })}
                  </span>
                </label>
              </div>
            ) : null}
            <div className="flex flex-wrap gap-2">
              {/* Runs only the steps that did not go through: the tenant is never created a second time. */}
              <Button
                size="sm"
                disabled={submitting || (outcomeUnknown(partial.error) && !partialChecked)}
                onClick={() => {
                  setPartial(null);
                  void submit();
                }}
              >
                {t("platform.create.partial_retry")}
              </Button>
              <Button asChild variant="outline" size="sm">
                <Link to={platformTenantPath(partial.created.id)}>{t("platform.create.open_tenant")}</Link>
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      </div>
    );
  }

  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={title} meta={t("platform.create.description")} crumbs={crumbs} />

      {step === "form" ? (
        <form noValidate onSubmit={review}>
          <Card>
            <CardHeader>
              <CardTitle>{t("platform.create.step1")}</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-5">
              <FormField
                label={t("platform.create.name")}
                description={t("platform.create.name_hint")}
                error={nameError}
              >
                {(control) => (
                  <Input
                    {...control}
                    value={name}
                    maxLength={NAME_MAX + 50}
                    onChange={(event) => setName(event.target.value)}
                  />
                )}
              </FormField>

              <fieldset className="grid gap-2" aria-labelledby={`${planId}-legend`}>
                <legend id={`${planId}-legend`} className="text-sm font-medium">
                  {t("platform.create.plan")}
                </legend>
                <div className="grid gap-3 sm:grid-cols-3">
                  {PLANS.map((option) => (
                    <label
                      key={option.plan}
                      className={cn(
                        "flex cursor-pointer items-start gap-2.5 rounded-md border px-3 py-2.5 text-sm",
                        plan === option.plan && "border-primary bg-accent",
                      )}
                    >
                      <input
                        type="radio"
                        name={`${planId}-plan`}
                        checked={plan === option.plan}
                        onChange={() => setPlan(option.plan)}
                        className="mt-0.5 size-4 accent-primary"
                      />
                      <span className="grid gap-0.5">
                        <span className="font-medium">{t(PLAN_LABELS[option.plan])}</span>
                        <span className="text-muted-foreground">{t(option.hint)}</span>
                      </span>
                    </label>
                  ))}
                </div>
              </fieldset>

              {plan === "enterprise" ? (
                <fieldset className="grid gap-3">
                  <legend className="text-sm font-medium">{t("platform.create.overrides")}</legend>
                  <p className="text-sm text-muted-foreground">{t("platform.create.overrides_hint")}</p>
                  <div className="grid gap-4 sm:grid-cols-2">
                    <OverrideField
                      label={t("platform.create.override.gateways")}
                      value={gateways}
                      onChange={setGateways}
                      invalid={limits.maxGateways === null}
                    />
                    <OverrideField
                      label={t("platform.create.override.members")}
                      value={members}
                      onChange={setMembers}
                      invalid={limits.maxMembers === null}
                    />
                    <OverrideField
                      label={t("platform.create.override.alerts")}
                      value={alerts}
                      onChange={setAlerts}
                      invalid={limits.maxAlertRules === null}
                    />
                    <OverrideField
                      label={t("platform.create.override.ai")}
                      value={ai}
                      onChange={setAi}
                      invalid={limits.aiMonthlyUsd === null}
                      usd
                    />
                  </div>
                </fieldset>
              ) : null}

              <FormField
                label={t("platform.create.email")}
                description={t("platform.create.email_hint")}
                error={emailError}
              >
                {(control) => (
                  <Input
                    {...control}
                    type="email"
                    autoComplete="off"
                    value={email}
                    onChange={(event) => setEmail(event.target.value)}
                  />
                )}
              </FormField>
            </CardContent>
            <CardFooter className="justify-end">
              <Button asChild variant="ghost">
                <Link to={PLATFORM_TENANTS_PATH}>{t("platform.dialog.cancel")}</Link>
              </Button>
              <Button type="submit">{t("platform.create.next")}</Button>
            </CardFooter>
          </Card>
        </form>
      ) : (
        <Card>
          <CardHeader>
            <CardTitle>{t("platform.create.step2")}</CardTitle>
          </CardHeader>
          <CardContent className="grid gap-4">
            <dl className="grid gap-x-6 gap-y-2 text-sm sm:grid-cols-[auto_1fr]">
              <dt className="text-muted-foreground">{t("platform.create.review.name")}</dt>
              <dd className="font-medium wrap-anywhere">{trimmed}</dd>
              <dt className="text-muted-foreground">{t("platform.create.review.plan")}</dt>
              <dd className="font-medium">{t(PLAN_LABELS[plan])}</dd>
              {Object.keys(overrides()).length > 0 ? (
                <>
                  <dt className="text-muted-foreground">{t("platform.create.review.overrides")}</dt>
                  <dd className="font-medium">
                    {[
                      limits.maxGateways != null
                        ? t("platform.create.review.gateways", { value: String(limits.maxGateways) })
                        : null,
                      limits.maxMembers != null
                        ? t("platform.create.review.members", { value: String(limits.maxMembers) })
                        : null,
                      limits.maxAlertRules != null
                        ? t("platform.create.review.alerts", { value: String(limits.maxAlertRules) })
                        : null,
                      limits.aiMonthlyUsd != null
                        ? t("platform.create.review.ai", { value: String(limits.aiMonthlyUsd) })
                        : null,
                    ]
                      .filter(Boolean)
                      .join(t("platform.create.review.separator"))}
                  </dd>
                </>
              ) : null}
              <dt className="text-muted-foreground">{t("platform.create.review.admin")}</dt>
              <dd className="font-medium wrap-anywhere">
                {t("platform.create.review.invite", { email: email.trim() })}
              </dd>
            </dl>
            <AuditNote>{t("platform.create.audit")}</AuditNote>
            {error !== null ? (
              <Alert variant="destructive">
                <AlertTitle>{errorMessage(error)}</AlertTitle>
                <AlertDescription>
                  <p>
                    {fieldError(error, "/name") ??
                      t(outcomeUnknown(error) ? "platform.create.unknown" : "platform.error.nothing_changed")}
                  </p>
                </AlertDescription>
              </Alert>
            ) : null}
            {unsure ? (
              <div className="grid gap-2 rounded-md border px-3 py-2 text-sm">
                <a
                  className="w-fit underline underline-offset-4"
                  href={PLATFORM_TENANTS_PATH}
                  target="_blank"
                  rel="noreferrer"
                >
                  {t("platform.create.open_list")}
                </a>
                <label className="flex items-start gap-2">
                  <input
                    type="checkbox"
                    className="mt-0.5 size-4 accent-primary"
                    checked={reconciled}
                    onChange={(event) => setReconciled(event.target.checked)}
                  />
                  <span>{t("platform.create.reconciled", { name: trimmed })}</span>
                </label>
              </div>
            ) : null}
          </CardContent>
          <CardFooter className="justify-end">
            <Button variant="ghost" disabled={submitting} onClick={() => setStep("form")}>
              {t("platform.create.back")}
            </Button>
            <Button disabled={submitting || (unsure && !reconciled)} onClick={() => void submit()}>
              {submitting ? <Spinner /> : null}
              {submitting ? t("platform.create.submitting") : t("platform.create.submit")}
            </Button>
          </CardFooter>
        </Card>
      )}
    </div>
  );
}

function OverrideField(props: {
  label: string;
  value: string;
  onChange(value: string): void;
  invalid: boolean;
  usd?: boolean;
}) {
  const { t } = useI18n();
  return (
    <FormField
      label={props.label}
      error={
        props.invalid ? t(props.usd ? "platform.quota.extra_invalid" : "platform.quota.gateways_invalid") : undefined
      }
    >
      {(control) => (
        <div className="flex items-center gap-2">
          {props.usd ? (
            <span aria-hidden="true" className="text-sm text-muted-foreground">
              US$
            </span>
          ) : null}
          <Input
            {...control}
            value={props.value}
            placeholder={t("platform.create.plan_default")}
            onChange={(event) => props.onChange(event.target.value)}
          />
        </div>
      )}
    </FormField>
  );
}
