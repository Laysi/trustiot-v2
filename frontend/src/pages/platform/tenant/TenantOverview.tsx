import { CircleCheckIcon, LoaderCircleIcon, TriangleAlertIcon } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router";

import type { EventScope, TenantSummary, Usage } from "../../../api/pending/platform.ts";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import { Button } from "../../../components/ui/button.tsx";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from "../../../components/ui/card.tsx";
import { useI18n } from "../../../i18n/useI18n.ts";
import { AuditEvents } from "../audit/AuditEvents.tsx";
import { formatUsd, isFinal, limitNumber, PLAN_LABELS, REASON_LABELS } from "../format.ts";
import { platformAuditPath } from "../paths.ts";
import { useUserNames } from "../queries.ts";
import { TenantStatusBadge } from "../TenantStatusBadge.tsx";
import { Meter } from "../ui/meter.tsx";
import { useStuck, WaitedFor } from "../Waiting.tsx";

export type OverviewAction = "reason" | "resume" | "quota";

/**
 * A tenant's overview (docs/design/platform-admin/tenant-detail.html): the state card, with the
 * timeline while a suspension is processing, the quota card and the latest operations.
 */
export function TenantOverview(props: {
  tenant: TenantSummary;
  onAction(action: OverviewAction, element: HTMLElement | null): void;
}) {
  const { t } = useI18n();
  const { tenant } = props;
  const scope: EventScope = { kind: "tenant", tenantId: tenant.id };
  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <StateCard tenant={tenant} onAction={props.onAction} />
      <QuotaCard tenant={tenant} onAction={props.onAction} />
      <Card className="lg:col-span-2">
        <CardHeader>
          <CardTitle>{t("platform.tenant.recent")}</CardTitle>
          <CardDescription>{t("platform.tenant.recent_description")}</CardDescription>
          <CardAction>
            <Button asChild variant="ghost" size="sm">
              <Link to={platformAuditPath(tenant.id)}>{t("platform.tenant.recent_all")}</Link>
            </Button>
          </CardAction>
        </CardHeader>
        <CardContent>
          <AuditEvents scope={scope} limit={3} />
        </CardContent>
      </Card>
    </div>
  );
}

function StateCard(props: {
  tenant: TenantSummary;
  onAction(action: OverviewAction, element: HTMLElement | null): void;
}) {
  const { t, formatDateTime } = useI18n();
  const { tenant } = props;
  const nameOf = useUserNames([tenant.changedByUserId]);
  const stuck = useStuck(tenant.stateChangedAt, tenant.status);
  const final = isFinal(tenant);
  const who = tenant.changedByUserId ? nameOf(tenant.changedByUserId) : t("platform.tenant.state.nobody");
  const reason = tenant.reason ? t(REASON_LABELS[tenant.reason]) : "";
  const created = tenant.stateVersion === 1 && tenant.changedByUserId === null;
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex flex-wrap items-center gap-2">
          {t("platform.tenant.state.title")}
          <TenantStatusBadge status={tenant.status} reason={tenant.reason} />
        </CardTitle>
        <CardDescription>{t("platform.tenant.state.description")}</CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4">
        {tenant.status === "processing" ? (
          <>
            <p className="text-sm">
              {t("platform.tenant.processing.lead")} <WaitedFor since={tenant.stateChangedAt} />{" "}
              {t("platform.tenant.processing.since", { time: formatDateTime(tenant.stateChangedAt) })}
            </p>
            <ol className="grid gap-3 text-sm">
              <Step done title={t("platform.tenant.step.recorded", { version: tenant.stateVersion })}>
                {t("platform.tenant.step.recorded_detail", {
                  time: formatDateTime(tenant.stateChangedAt),
                  who,
                  reason,
                })}
              </Step>
              <Step done title={t("platform.tenant.step.blocked")}>
                {t("platform.tenant.step.blocked_detail")}
              </Step>
              <Step title={t("platform.tenant.step.waiting")}>{t("platform.tenant.step.waiting_detail")}</Step>
            </ol>
            {stuck ? (
              <Alert role="status" className="grid-cols-[1rem_1fr] border-warning/50 bg-warning-soft text-foreground">
                <TriangleAlertIcon aria-hidden="true" />
                <AlertTitle>{t("platform.tenant.stuck_title")}</AlertTitle>
                <AlertDescription className="text-foreground">
                  <p>{t("platform.tenant.stuck_body")}</p>
                  <Link className="text-sm underline underline-offset-4" to={platformAuditPath(tenant.id)}>
                    {t("platform.tenant.stuck_log")}
                  </Link>
                </AlertDescription>
              </Alert>
            ) : null}
          </>
        ) : null}
        <dl className="grid gap-x-6 gap-y-2 text-sm sm:grid-cols-[auto_1fr]">
          {tenant.reason ? (
            <>
              <dt className="text-muted-foreground">{t("platform.tenant.state.reason")}</dt>
              <dd className="font-medium">{reason}</dd>
            </>
          ) : null}
          <dt className="text-muted-foreground">
            {tenant.status === "active" ? t("platform.tenant.state.since") : t("platform.tenant.state.suspended_at")}
          </dt>
          <dd>
            {formatDateTime(tenant.stateChangedAt)}
            {created ? ` ${t("platform.tenant.state.created")}` : ""}
          </dd>
          <dt className="text-muted-foreground">{t("platform.tenant.state.version")}</dt>
          <dd className="tabular-nums">{tenant.stateVersion}</dd>
          <dt className="text-muted-foreground">{t("platform.tenant.state.by")}</dt>
          <dd className="wrap-anywhere">{created ? t("platform.tenant.state.unchanged") : who}</dd>
        </dl>
        {final ? <p className="text-sm text-muted-foreground">{t("platform.tenant.final_note")}</p> : null}
      </CardContent>
      {!final && tenant.status !== "active" ? (
        <CardFooter className="flex-wrap gap-2">
          {tenant.status === "suspended" ? (
            <Button variant="outline" size="sm" onClick={(event) => props.onAction("reason", event.currentTarget)}>
              {t("platform.tenant.action.reason")}
            </Button>
          ) : null}
          <Button variant="outline" size="sm" onClick={(event) => props.onAction("resume", event.currentTarget)}>
            {t("platform.tenant.action.resume")}
          </Button>
        </CardFooter>
      ) : null}
    </Card>
  );
}

function Step(props: { title: string; done?: boolean; children: ReactNode }) {
  const { t } = useI18n();
  return (
    <li className="flex items-start gap-2.5">
      {props.done ? (
        <CircleCheckIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 text-success" />
      ) : (
        <LoaderCircleIcon aria-hidden="true" className="mt-0.5 size-4 shrink-0 animate-spin text-warning" />
      )}
      <span className="grid gap-0.5">
        <span className="font-medium">
          {props.title}
          <span className="sr-only">
            {" "}
            ({t(props.done ? "platform.tenant.step.done" : "platform.tenant.step.pending")})
          </span>
        </span>
        <span className="text-xs text-muted-foreground">{props.children}</span>
      </span>
    </li>
  );
}

function UsageRow({ label, usage }: { label: string; usage: Usage }) {
  const { t } = useI18n();
  return (
    <div className="grid gap-1">
      <div className="flex items-baseline justify-between gap-3 text-sm">
        <span>{label}</span>
        <span className="tabular-nums">
          {usage.used} / {usage.limit === "unlimited" ? t("platform.limit.unlimited") : usage.limit}
        </span>
      </div>
      {usage.limit !== "unlimited" ? <Meter value={usage.used} max={limitNumber(usage.limit)} label={label} /> : null}
    </div>
  );
}

function QuotaCard(props: {
  tenant: TenantSummary;
  onAction(action: OverviewAction, element: HTMLElement | null): void;
}) {
  const { t, locale } = useI18n();
  const { tenant } = props;
  const ai = tenant.ai;
  return (
    <Card>
      <CardHeader>
        <CardTitle>{t("platform.tenant.quota.title")}</CardTitle>
        <CardDescription>
          {t("platform.tenant.quota.description", { plan: t(PLAN_LABELS[tenant.plan]) })}
        </CardDescription>
        {isFinal(tenant) ? null : (
          <CardAction>
            <Button variant="ghost" size="sm" onClick={(event) => props.onAction("quota", event.currentTarget)}>
              {t("platform.tenant.quota.adjust")}
            </Button>
          </CardAction>
        )}
      </CardHeader>
      <CardContent className="grid gap-3">
        <UsageRow label={t("platform.tenant.quota.gateways")} usage={tenant.gateways} />
        <UsageRow label={t("platform.tenant.quota.members")} usage={tenant.members} />
        <UsageRow label={t("platform.tenant.quota.alerts")} usage={tenant.alertRules} />
        <div className="grid gap-1">
          <div className="flex items-baseline justify-between gap-3 text-sm">
            <span>{t("platform.tenant.quota.ai")}</span>
            <span className="tabular-nums">
              {formatUsd(ai.usedUsd, locale)} / {formatUsd(ai.planLimitUsd, locale)}
              {ai.extraUsd > 0 ? ` + ${formatUsd(ai.extraUsd, locale)}` : ""}
            </span>
          </div>
          <Meter value={ai.usedUsd} max={ai.limitUsd} label={t("platform.tenant.quota.ai")} />
          {ai.extraUsd > 0 && ai.extraUntil ? (
            <p className="text-xs text-muted-foreground">
              {t("platform.tenant.quota.extra_note", {
                extra: formatUsd(ai.extraUsd, locale),
                until: ai.extraUntil,
                plan: formatUsd(ai.planLimitUsd, locale),
              })}
            </p>
          ) : null}
        </div>
      </CardContent>
    </Card>
  );
}
