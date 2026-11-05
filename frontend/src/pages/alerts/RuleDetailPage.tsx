import { useQuery } from "@tanstack/react-query";
import { CircleCheckIcon, CircleOffIcon, CircleXIcon, PlayIcon, SearchXIcon } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { Link, useLocation, useParams, useSearchParams } from "react-router";

import { useAlertsApi } from "../../api/pending/AlertsApiProvider.tsx";
import type { Backtest, RuleDetail, RuleOrigin, RuleVersion } from "../../api/pending/alerts.ts";
import { ApiError } from "../../api/errors.ts";
import { devicePath } from "../../app/paths.ts";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import { Spinner } from "../../components/ui/spinner.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { StatusPage } from "../StatusPage.tsx";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { useEnableRule } from "./actions.ts";
import { BacktestChart } from "./BacktestChart.tsx";
import { DisableRuleDialog } from "./DisableRuleDialog.tsx";
import { diffLines } from "./diff.ts";
import { ALERTS_PATH, alertsPath } from "./paths.ts";
import { ActionError, AskAiButton, ListSkeleton, OriginBadge, Section, StateBadge } from "./parts.tsx";
import { overviewKey, ruleKey } from "./queries.ts";
import { ruleState, useAlertsRole, useTenantId } from "./state.ts";
import { useAlertsTime } from "./time.ts";

const VERSION_PARAM = "version";
const ACTION_PARAM = "action";

/** Who wrote a version, for a sentence: "AI" or the member's name. */
function authorName(origin: RuleOrigin, ai: string): string {
  return origin.kind === "ai" ? ai : origin.name;
}

/**
 * One rule (`/alerts/rules/:ruleId`, design 「規則詳情」): what it does in plain words with the
 * script folded away, the backtest that let it start (each of the four checks, the daily trigger
 * bars and the triggers as a table), the version history with the differences between two
 * versions, and on the right its notification settings, where it applies and its recent
 * triggers. `?version=<n>` opens that version of the history; `?action=disable` opens the disable
 * confirmation, the target of the notification Email's link, which disables nothing by itself.
 */
export function RuleDetailPage() {
  const { t, errorMessage } = useI18n();
  const { ruleId = "" } = useParams();
  const api = useAlertsApi();
  const tenant = useTenantId();
  const { canManage } = useAlertsRole();
  const [params, setParams] = useSearchParams();
  const [actionError, setActionError] = useState<unknown>(null);
  const enable = useEnableRule();

  const rule = useQuery({
    queryKey: ruleKey(tenant, ruleId),
    queryFn: ({ signal }) => api.getRule(ruleId, signal),
    retry: false,
  });
  const overview = useQuery({
    queryKey: overviewKey(tenant),
    queryFn: ({ signal }) => api.getOverview(signal),
  });

  // The page renders a skeleton first, so a link to a section (`#recent`, `#versions`) scrolls once the rule is there.
  const { hash } = useLocation();
  const loaded = rule.isSuccess;
  useEffect(() => {
    if (loaded && hash.length > 1) document.getElementById(decodeURIComponent(hash.slice(1)))?.scrollIntoView?.();
  }, [loaded, hash]);

  const fromEmail = params.get(ACTION_PARAM) === "disable";
  const detail = rule.data;
  // The link opens the confirmation only where it can be used: an editor, a rule that is on.
  const dialogOpen = fromEmail && canManage && detail?.status === "active";
  const [manualOpen, setManualOpen] = useState(false);
  const setDialogOpen = (open: boolean) => {
    setManualOpen(open);
    if (!open && fromEmail) {
      setParams(
        (current) => {
          const next = new URLSearchParams(current);
          next.delete(ACTION_PARAM);
          return next;
        },
        { replace: true },
      );
    }
  };

  if (rule.isPending) {
    return (
      <div className="grid gap-4">
        <ListSkeleton label={t("alerts.rule.loading")} />
      </div>
    );
  }
  if (rule.isError) {
    if (rule.error instanceof ApiError && rule.error.status === 404) {
      return (
        <StatusPage
          title={t("alerts.rule.not_found.title")}
          icon={<SearchXIcon />}
          actions={
            <Button asChild variant="outline">
              <Link to={alertsPath("rules")}>{t("alerts.rule.back_to_rules")}</Link>
            </Button>
          }
        >
          <p>{t("alerts.rule.not_found.body")}</p>
        </StatusPage>
      );
    }
    return (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("alerts.rule.load_failed", { reason: errorMessage(rule.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void rule.refetch()} disabled={rule.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  }

  const state = ruleState(rule.data, overview.data?.pause);
  const current = rule.data.versions[0];
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader
        title={rule.data.name}
        crumbs={[
          { label: t("alerts.title"), to: ALERTS_PATH },
          { label: t("alerts.tab.rules"), to: alertsPath("rules") },
        ]}
        tags={
          <>
            <OriginBadge origin={rule.data.origin} showName />
            <StateBadge state={state} />
          </>
        }
        meta={<VersionLine rule={rule.data} current={current} />}
        actions={
          <>
            <AskAiButton
              question={t("alerts.ask.explain_rule")}
              context={[t("alerts.ask.context_rule", { rule: rule.data.name }), t("alerts.ask.context_version", { version: rule.data.version })]}
            />
            {canManage && rule.data.status === "active" ? (
              <Button
                variant="outline"
                className="border-destructive/50 text-destructive hover:bg-destructive/10 hover:text-destructive"
                onClick={() => setDialogOpen(true)}
              >
                <CircleOffIcon aria-hidden="true" />
                {t("alerts.rule.disable")}
              </Button>
            ) : null}
            {canManage && rule.data.status === "disabled" ? (
              <Button
                variant="outline"
                disabled={enable.isPending}
                onClick={() => enable.mutate(rule.data.id, { onError: setActionError })}
              >
                {enable.isPending ? <Spinner /> : <PlayIcon aria-hidden="true" />}
                {t("alerts.rule.enable")}
              </Button>
            ) : null}
          </>
        }
      />
      {canManage ? null : <p className="text-sm text-muted-foreground">{t("alerts.needs_editor")}</p>}
      {actionError !== null ? (
        <ActionError message={errorMessage(actionError)} onDismiss={() => setActionError(null)} />
      ) : null}
      {rule.data.status === "disabled" && rule.data.disabled ? <DisabledNote rule={rule.data} /> : null}

      <div className="grid items-start gap-4 lg:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
        <div className="grid gap-4">
          <Section id="what" title={t("alerts.rule.what")}>
            <p>{rule.data.description}</p>
            <details className="text-sm">
              <summary className={cn("w-fit cursor-pointer rounded-sm", focusRing)}>{t("alerts.rule.script")}</summary>
              <pre className="mt-2 overflow-x-auto rounded-md bg-muted p-3 font-mono text-xs">{rule.data.script}</pre>
            </details>
          </Section>
          {current ? <BacktestSection version={current} /> : null}
          <VersionHistory versions={rule.data.versions} current={rule.data.version} />
        </div>
        <div className="grid gap-4">
          <NotificationSettings rule={rule.data} />
          <Section id="scope" title={t("alerts.rule.scope")}>
            {rule.data.scope.length === 0 ? (
              <p className="text-sm text-muted-foreground">{t("alerts.rule.scope_none")}</p>
            ) : (
              <ul className="grid gap-2 text-sm">
                {rule.data.scope.map((item) => (
                  <li key={`${item.device_id}/${item.point}`}>
                    <Link
                      to={`${devicePath(item.device_id)}?point=${encodeURIComponent(item.point)}`}
                      className={cn("rounded-sm underline underline-offset-4", focusRing)}
                    >
                      {t("alerts.rule.scope_item", { device: item.device_name, point: item.point })}
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </Section>
          <RecentTriggers rule={rule.data} />
        </div>
      </div>

      <DisableRuleDialog
        rule={dialogOpen || manualOpen ? { id: rule.data.id, name: rule.data.name } : null}
        fromEmail={fromEmail}
        onOpenChange={setDialogOpen}
      />
    </div>
  );
}

function VersionLine({ rule, current }: { rule: RuleDetail; current: RuleVersion | undefined }) {
  const { t } = useI18n();
  const time = useAlertsTime();
  if (!current) return <>{`v${rule.version}`}</>;
  const author = authorName(current.author, t("alerts.origin.ai_short"));
  return (
    <>
      {t(current.version === 1 ? "alerts.rule.version_created" : "alerts.rule.version_modified", {
        version: current.version,
        author,
        time: time.full(current.created_at),
      })}
    </>
  );
}

function DisabledNote({ rule }: { rule: RuleDetail }) {
  const { t } = useI18n();
  const time = useAlertsTime();
  if (!rule.disabled) return null;
  return (
    <Alert role="status" className="grid-cols-[1rem_1fr]">
      <CircleOffIcon aria-hidden="true" />
      <AlertDescription>
        <p>{t("alerts.rule.disabled_note", { name: rule.disabled.by, time: time.full(rule.disabled.at) })}</p>
        {rule.disabled.reason ? <p>{t("alerts.rule.disabled_reason", { reason: rule.disabled.reason })}</p> : null}
      </AlertDescription>
    </Alert>
  );
}

function CheckItem({ ok, children }: { ok: boolean; children: ReactNode }) {
  const { t } = useI18n();
  return (
    <li className="grid grid-cols-[1.25rem_1fr] items-start gap-x-2 text-sm">
      {ok ? (
        <CircleCheckIcon aria-hidden="true" className="mt-0.5 size-4 text-success" />
      ) : (
        <CircleXIcon aria-hidden="true" className="mt-0.5 size-4 text-destructive" />
      )}
      <span>
        <span className="sr-only">{t(ok ? "alerts.backtest.check_ok" : "alerts.backtest.check_failed")} </span>
        {children}
      </span>
    </li>
  );
}

function casesText(backtest: Backtest, time: ReturnType<typeof useAlertsTime>, t: ReturnType<typeof useI18n>["t"]): string {
  return backtest.cases
    .map((item) =>
      item.source === "history" && item.at !== undefined
        ? t("alerts.backtest.case_history", { time: time.when(item.at) })
        : t("alerts.backtest.case_synthetic"),
    )
    .join(", ");
}

/** The backtest of the current version: the four checks of Laysi/trustiot-v2#168, the daily bars and the triggers as a table. */
function BacktestSection({ version }: { version: RuleVersion }) {
  const { t, formatNumber } = useI18n();
  const time = useAlertsTime();
  const [table, setTable] = useState(false);
  const backtest = version.backtest;
  const fired = backtest.cases.filter((item) => item.triggered).length;
  return (
    <Section
      id="backtest"
      title={t("alerts.backtest.title")}
      note={t("alerts.backtest.window", {
        version: version.version,
        from: time.day(backtest.from),
        to: time.day(backtest.to),
      })}
      aside={
        backtest.passed ? (
          <span className="inline-flex items-center gap-1 text-sm font-medium">
            <CircleCheckIcon aria-hidden="true" className="size-4 text-success" />
            {t("alerts.backtest.passed")}
          </span>
        ) : (
          <span className="inline-flex items-center gap-1 rounded-sm border border-destructive/50 px-1.5 py-0.5 text-sm font-medium text-destructive">
            <CircleXIcon aria-hidden="true" className="size-4" />
            {t("alerts.backtest.failed")}
          </span>
        )
      }
    >
      <ul className="grid gap-2">
        <CheckItem ok={backtest.syntax_ok}>{t("alerts.backtest.syntax")}</CheckItem>
        <CheckItem ok={backtest.execution_ok}>{t("alerts.backtest.execution")}</CheckItem>
        <CheckItem ok={backtest.trigger_count <= backtest.trigger_limit}>
          {t("alerts.backtest.trigger_count", {
            count: formatNumber(backtest.trigger_count),
            limit: formatNumber(backtest.trigger_limit),
          })}
        </CheckItem>
        <CheckItem ok={backtest.cases.length > 0 && fired === backtest.cases.length}>
          {backtest.cases.length === 0
            ? t("alerts.backtest.cases_none")
            : t("alerts.backtest.cases", {
                fired: formatNumber(fired),
                total: formatNumber(backtest.cases.length),
                details: casesText(backtest, time, t),
              })}
        </CheckItem>
      </ul>
      {backtest.history_days < 14 ? (
        <p className="text-sm text-muted-foreground">{t("alerts.backtest.short_history", { days: backtest.history_days })}</p>
      ) : null}
      <BacktestChart backtest={backtest} />
      <div className="grid gap-2">
        <Button
          variant="link"
          size="sm"
          className="justify-self-start"
          aria-expanded={table}
          aria-controls="backtest-triggers"
          onClick={() => setTable(!table)}
        >
          {table ? t("alerts.backtest.hide_table") : t("alerts.backtest.show_table")}
        </Button>
        {table ? (
          <div id="backtest-triggers">
            {backtest.triggers.length === 0 ? (
              <p className="text-sm text-muted-foreground">{t("alerts.backtest.table_empty")}</p>
            ) : (
              <table className="w-full text-sm">
                <caption className="sr-only">{t("alerts.backtest.table_caption", { count: backtest.triggers.length })}</caption>
                <thead className="text-left text-muted-foreground">
                  <tr>
                    <th scope="col" className="py-1 pr-3 font-medium">
                      {t("alerts.backtest.table.time")}
                    </th>
                    <th scope="col" className="py-1 pr-3 font-medium">
                      {t("alerts.backtest.table.device")}
                    </th>
                    <th scope="col" className="py-1 font-medium">
                      {t("alerts.backtest.table.what")}
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {backtest.triggers.map((trigger) => (
                    <tr key={`${trigger.at}/${trigger.device_name}`} className="border-t">
                      <td className="py-1.5 pr-3 whitespace-nowrap">
                        <time dateTime={trigger.at}>{time.full(trigger.at)}</time>
                      </td>
                      <td className="py-1.5 pr-3">{trigger.device_name}</td>
                      <td className="py-1.5 wrap-anywhere">{trigger.summary}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        ) : null}
      </div>
    </Section>
  );
}

/** The versions newest first as a list; one version, named in the address, is open with its differences from the one before. */
function VersionHistory({ versions, current }: { versions: readonly RuleVersion[]; current: number }) {
  const { t } = useI18n();
  const time = useAlertsTime();
  const [params, setParams] = useSearchParams();
  const requested = Number(params.get(VERSION_PARAM));
  const open = versions.some((item) => item.version === requested) ? requested : null;
  const toggle = (version: number) =>
    setParams(
      (existing) => {
        const next = new URLSearchParams(existing);
        if (open === version) next.delete(VERSION_PARAM);
        else next.set(VERSION_PARAM, String(version));
        return next;
      },
      { replace: true },
    );
  useEffect(() => {
    if (open !== null) document.getElementById(`version-${open}`)?.scrollIntoView?.({ block: "center" });
  }, [open]);
  return (
    <Section id="versions" title={t("alerts.versions.title")}>
      <ol className="grid gap-4">
        {versions.map((version, index) => {
          const before = versions[index + 1];
          const expanded = open === version.version;
          return (
            <li
              key={version.version}
              id={`version-${version.version}`}
              aria-current={version.version === current ? "true" : undefined}
              className={cn("grid gap-2 border-l-2 pl-4", version.version === current ? "border-primary" : "border-border")}
            >
              <div className="flex flex-wrap items-center justify-between gap-2">
                <p className="flex flex-wrap items-center gap-2">
                  <b>v{version.version}</b>
                  {version.version === current ? <span className="text-sm">{t("alerts.versions.current")}</span> : null}
                  <OriginBadge origin={version.author} showName />
                </p>
                <time dateTime={version.created_at} className="text-sm text-muted-foreground">
                  {time.full(version.created_at)}
                </time>
              </div>
              <p className="text-sm">{version.summary}</p>
              <div className="flex flex-wrap items-center gap-3 text-sm">
                {version.backtest.passed ? (
                  <span className="inline-flex items-center gap-1">
                    <CircleCheckIcon aria-hidden="true" className="size-4 text-success" />
                    {t("alerts.versions.backtest_passed")}
                  </span>
                ) : (
                  <span className="inline-flex items-center gap-1 text-destructive">
                    <CircleXIcon aria-hidden="true" className="size-4" />
                    {t("alerts.versions.backtest_failed")}
                  </span>
                )}
                {before ? (
                  <Button
                    variant="link"
                    size="sm"
                    aria-expanded={expanded}
                    onClick={() => toggle(version.version)}
                  >
                    {t("alerts.versions.compare", { before: before.version, after: version.version })}
                  </Button>
                ) : (
                  <Button variant="link" size="sm" aria-expanded={expanded} onClick={() => toggle(version.version)}>
                    {t("alerts.versions.show_script")}
                  </Button>
                )}
              </div>
              {expanded ? <VersionBody version={version} before={before} /> : null}
            </li>
          );
        })}
      </ol>
    </Section>
  );
}

/** The differences from the version before, with `-` and `+` in front of the lines and a tint, so colour is not the only sign. */
function VersionBody({ version, before }: { version: RuleVersion; before: RuleVersion | undefined }) {
  const { t } = useI18n();
  const lines = useMemo(
    () => (before ? diffLines(before.script, version.script) : diffLines(version.script, version.script)),
    [before, version],
  );
  return (
    <pre
      aria-label={
        before
          ? t("alerts.versions.diff_label", { before: before.version, after: version.version })
          : t("alerts.versions.script_label", { version: version.version })
      }
      className="overflow-x-auto rounded-md bg-muted py-2 font-mono text-xs"
    >
      {lines.map((line, index) => (
        // The script is fixed text, so a line's place is its identity.
        // biome-ignore lint/suspicious/noArrayIndexKey: lines have no other identity
        <span
          key={index}
          className={cn(
            "block px-3 whitespace-pre",
            line.kind === "del" && "bg-destructive/15",
            line.kind === "add" && "bg-success/15",
          )}
        >
          {before ? (line.kind === "del" ? "- " : line.kind === "add" ? "+ " : "  ") : ""}
          {line.text}
        </span>
      ))}
    </pre>
  );
}

function NotificationSettings({ rule }: { rule: RuleDetail }) {
  const { t, formatNumber } = useI18n();
  const channels: string[] = [];
  if (rule.channels.in_app) channels.push(t("alerts.rule.channel_in_app"));
  if (rule.channels.email_recipients > 0) {
    channels.push(t("alerts.rule.channel_email", { count: formatNumber(rule.channels.email_recipients) }));
  }
  for (const name of rule.channels.webhook_names) channels.push(t("alerts.rule.channel_webhook", { name }));
  return (
    <Section id="notify" title={t("alerts.rule.notify")}>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-sm">
        <dt className="text-muted-foreground">{t("alerts.rule.cooldown")}</dt>
        <dd>{t("alerts.rule.cooldown_value", { minutes: formatNumber(rule.cooldown_minutes) })}</dd>
        <dt className="text-muted-foreground">{t("alerts.rule.hourly_limit")}</dt>
        <dd>{t("alerts.rule.hourly_limit_value", { count: formatNumber(rule.hourly_limit) })}</dd>
        <dt className="text-muted-foreground">{t("alerts.rule.channels")}</dt>
        <dd>
          <ul className="grid gap-1">
            {channels.map((channel) => (
              <li key={channel}>{channel}</li>
            ))}
          </ul>
        </dd>
      </dl>
    </Section>
  );
}

function RecentTriggers({ rule }: { rule: RuleDetail }) {
  const { t } = useI18n();
  const time = useAlertsTime();
  return (
    <Section id="recent" title={t("alerts.rule.recent")}>
      {rule.recent_triggers.length === 0 ? (
        <p className="text-sm text-muted-foreground">{t("alerts.rule.recent_none")}</p>
      ) : (
        <ul className="grid gap-3">
          {rule.recent_triggers.map((trigger) => (
            <li key={`${trigger.at}/${trigger.device_id}`} className="grid gap-1.5 text-sm">
              <p className="flex flex-wrap items-baseline justify-between gap-2">
                <span>{t("alerts.rule.recent_item", { device: trigger.device_name, what: trigger.summary })}</span>
                <time dateTime={trigger.at} className="text-muted-foreground">
                  {time.when(trigger.at)}
                </time>
              </p>
              <div>
                <AskAiButton
                  size="sm"
                  question={t("alerts.ask.analyze_stretch")}
                  label={t("alerts.ask.analyze_stretch_at", { device: trigger.device_name, time: time.full(trigger.at) })}
                  context={[
                    t("alerts.ask.context_rule", { rule: rule.name }),
                    t("alerts.ask.context_device", { device: trigger.device_name }),
                    t("alerts.ask.context_around", { time: time.full(trigger.at) }),
                  ]}
                />
              </div>
            </li>
          ))}
        </ul>
      )}
      <div className="border-t pt-3">
        <AskAiButton
          size="sm"
          question={t("alerts.ask.compare_last_week")}
          context={[t("alerts.ask.context_rule", { rule: rule.name }), t("alerts.ask.context_version", { version: rule.version })]}
        />
      </div>
    </Section>
  );
}
