import { useQuery } from "@tanstack/react-query";
import { CircleOffIcon, EllipsisIcon, PlayIcon, SearchIcon, SearchXIcon, SparklesIcon } from "lucide-react";
import { type ReactNode, useId } from "react";
import { Link, useSearchParams } from "react-router";

import { useAlertsApi } from "../../api/pending/AlertsApiProvider.tsx";
import type { AiPause, RuleSummary } from "../../api/pending/alerts.ts";
import { EmptyState } from "../../components/EmptyState.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button, focusRing } from "../../components/ui/button.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "../../components/ui/dropdown-menu.tsx";
import { controlClasses } from "../../components/ui/input.tsx";
import { NativeSelect } from "../../components/ui/native-select.tsx";
import type { MessageId } from "../../i18n/messages.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { useEnableRule } from "./actions.ts";
import { AskAiButton, ListSkeleton, OriginBadge, Segmented, StateBadge } from "./parts.tsx";
import { rulePath } from "./paths.ts";
import { rulesKey } from "./queries.ts";
import { ruleState, useAlertsRole, useTenantId } from "./state.ts";
import { useAlertsTime } from "./time.ts";

const ORIGIN_PARAM = "origin";
const STATUS_PARAM = "status";
const SEARCH_PARAM = "q";

type OriginFilter = "all" | "ai" | "member";
const STATUS_FILTERS = ["active", "disabled", "backtest_failed"] as const;

const FAILURE_TEXT: Record<NonNullable<RuleSummary["backtest_failure"]>, MessageId> = {
  syntax: "alerts.failure.syntax",
  execution: "alerts.failure.execution",
  trigger_count: "alerts.failure.trigger_count",
  expected_case: "alerts.failure.expected_case",
};

/**
 * The rule list (design 「規則列表」): who made each rule, its state (a disabled one with who and
 * when, a refused one with the failed check, an AI-made one while every AI rule is paused as "paused",
 * apart from a single disabled rule), where it applies and its newest trigger. The row menu
 * disables and enables for an editor; a viewer sees the same list without the menu.
 */
export function RulesTab({
  pause,
  onDisable,
  onError,
}: {
  pause: AiPause;
  onDisable(rule: { id: string; name: string }): void;
  onError(error: unknown): void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useAlertsApi();
  const tenant = useTenantId();
  const { canManage } = useAlertsRole();
  const enable = useEnableRule();
  const [params, setParams] = useSearchParams();
  const searchId = useId();
  const origin: OriginFilter =
    params.get(ORIGIN_PARAM) === "ai" ? "ai" : params.get(ORIGIN_PARAM) === "member" ? "member" : "all";
  const statusParam = params.get(STATUS_PARAM);
  const status = STATUS_FILTERS.find((value) => value === statusParam) ?? "";
  const search = (params.get(SEARCH_PARAM) ?? "").trim().toLowerCase();

  const rules = useQuery({
    queryKey: rulesKey(tenant),
    queryFn: ({ signal }) => api.listRules(signal),
  });

  const setParam = (name: string, value: string) =>
    setParams(
      (current) => {
        const next = new URLSearchParams(current);
        if (value === "") next.delete(name);
        else next.set(name, value);
        return next;
      },
      { replace: true },
    );

  const all = rules.data ?? [];
  const shown = all.filter(
    (rule) =>
      (origin === "all" || rule.origin.kind === origin) &&
      (status === "" || rule.status === status) &&
      (search === "" ||
        rule.name.toLowerCase().includes(search) ||
        rule.applies_to.some((name) => name.toLowerCase().includes(search))),
  );

  let body: ReactNode;
  if (rules.isPending) {
    body = <ListSkeleton label={t("alerts.rules.loading")} />;
  } else if (rules.isError) {
    body = (
      <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
        <AlertDescription className="col-start-1">
          <p>{t("alerts.rules.load_failed", { reason: errorMessage(rules.error) })}</p>
        </AlertDescription>
        <Button variant="outline" onClick={() => void rules.refetch()} disabled={rules.isFetching}>
          {t("page.action.retry")}
        </Button>
      </Alert>
    );
  } else if (all.length === 0) {
    body = (
      <EmptyState
        icon={<SparklesIcon />}
        title={t("alerts.rules.empty.title")}
        action={
          <AskAiButton
            question={t("alerts.ask.suggest_rules")}
            context={[t("alerts.ask.context_all_devices"), t("alerts.ask.context_14_days")]}
          />
        }
      >
        <p>{t("alerts.rules.empty.body")}</p>
      </EmptyState>
    );
  } else if (shown.length === 0) {
    body = (
      <div role="status" className="grid justify-items-center gap-2 rounded-lg border px-4 py-12 text-center">
        <SearchXIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <p className="font-semibold">{t("alerts.rules.no_match.title")}</p>
        <p className="text-sm text-muted-foreground">{t("alerts.rules.no_match.body")}</p>
      </div>
    );
  } else {
    body = (
      <RuleList
        rules={shown}
        pause={pause}
        canManage={canManage}
        onDisable={onDisable}
        onEnable={(id) => enable.mutate(id, { onError })}
        enabling={enable.isPending}
      />
    );
  }

  return (
    <div className="grid gap-4">
      {all.length > 0 ? (
        <search className="flex flex-wrap items-center gap-3">
          <Segmented
            label={t("alerts.rules.filter_origin")}
            value={origin}
            onChange={(value) => setParam(ORIGIN_PARAM, value === "all" ? "" : value)}
            options={[
              { id: "all", label: t("alerts.rules.origin_all") },
              { id: "ai", label: t("alerts.origin.ai") },
              { id: "member", label: t("alerts.origin.member") },
            ]}
          />
          <div className="w-full sm:w-44">
            <label htmlFor={`${searchId}-status`} className="sr-only">
              {t("alerts.rules.filter_status")}
            </label>
            <NativeSelect
              id={`${searchId}-status`}
              value={status}
              onChange={(event) => setParam(STATUS_PARAM, event.target.value)}
            >
              <option value="">{t("alerts.rules.status_all")}</option>
              <option value="active">{t("alerts.state.active")}</option>
              <option value="disabled">{t("alerts.state.disabled")}</option>
              <option value="backtest_failed">{t("alerts.state.backtest_failed")}</option>
            </NativeSelect>
          </div>
          <div className="relative w-full sm:w-64">
            <label htmlFor={searchId} className="sr-only">
              {t("alerts.rules.search_label")}
            </label>
            <SearchIcon
              aria-hidden="true"
              className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground"
            />
            <input
              id={searchId}
              type="search"
              defaultValue={params.get(SEARCH_PARAM) ?? ""}
              placeholder={t("alerts.rules.search_placeholder")}
              className={cn(controlClasses, "pl-9")}
              onChange={(event) => setParam(SEARCH_PARAM, event.target.value.trim())}
            />
          </div>
        </search>
      ) : null}
      {body}
      {all.length > 0 ? <p className="text-sm text-muted-foreground">{t("alerts.rules.failed_note")}</p> : null}
    </div>
  );
}

function RuleList({
  rules,
  pause,
  canManage,
  onDisable,
  onEnable,
  enabling,
}: {
  rules: readonly RuleSummary[];
  pause: AiPause;
  canManage: boolean;
  onDisable(rule: { id: string; name: string }): void;
  onEnable(id: string): void;
  enabling: boolean;
}) {
  const { t } = useI18n();
  const time = useAlertsTime();
  const lastTrigger = (rule: RuleSummary) =>
    rule.last_triggered_at === null ? (
      "—"
    ) : (
      <time dateTime={rule.last_triggered_at}>{time.when(rule.last_triggered_at)}</time>
    );
  const stateNote = (rule: RuleSummary): string | null => {
    if (rule.status === "disabled" && rule.disabled) {
      return t("alerts.rules.disabled_by", { name: rule.disabled.by, time: time.when(rule.disabled.at) });
    }
    if (rule.status === "backtest_failed" && rule.backtest_failure) return t(FAILURE_TEXT[rule.backtest_failure]);
    return null;
  };
  const menu = (rule: RuleSummary) =>
    canManage ? <RuleMenu rule={rule} onDisable={onDisable} onEnable={onEnable} enabling={enabling} /> : null;
  const link = (rule: RuleSummary) => (
    <Link
      to={rulePath(rule.id)}
      className={cn("rounded-sm font-medium wrap-anywhere underline-offset-4 hover:underline", focusRing)}
    >
      {rule.name}
    </Link>
  );
  return (
    <section aria-label={t("alerts.rules.list_label")} className="rounded-lg border bg-card text-card-foreground">
      <table className="hidden w-full text-sm sm:table">
        <caption className="sr-only">{t("alerts.rules.caption", { count: rules.length })}</caption>
        <thead className="bg-muted/60 text-left text-muted-foreground">
          <tr>
            <th scope="col" className="px-4 py-2.5 font-medium">
              {t("alerts.rules.column.rule")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("alerts.rules.column.origin")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("alerts.rules.column.state")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("alerts.rules.column.applies")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium whitespace-nowrap">
              {t("alerts.rules.column.last")}
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">
              {t("alerts.rules.column.version")}
            </th>
            <th scope="col" className="px-3 py-2.5">
              <span className="sr-only">{t("alerts.rules.column.actions")}</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {rules.map((rule) => {
            const note = stateNote(rule);
            return (
              <tr key={rule.id} className="border-t align-top">
                <th scope="row" className="max-w-64 px-4 py-3 text-left font-normal">
                  {link(rule)}
                </th>
                <td className="px-3 py-3">
                  <OriginBadge origin={rule.origin} showName />
                </td>
                <td className="px-3 py-3">
                  <div className="grid justify-items-start gap-1">
                    <StateBadge state={ruleState(rule, pause)} />
                    {note ? <span className="text-xs text-muted-foreground">{note}</span> : null}
                  </div>
                </td>
                <td className="max-w-56 px-3 py-3 wrap-anywhere">{rule.applies_to.join(", ")}</td>
                <td className="px-3 py-3 whitespace-nowrap">{lastTrigger(rule)}</td>
                <td className="px-3 py-3 whitespace-nowrap">v{rule.version}</td>
                <td className="px-3 py-3 text-right">{menu(rule)}</td>
              </tr>
            );
          })}
        </tbody>
      </table>
      <ul aria-label={t("alerts.rules.caption", { count: rules.length })} className="grid divide-y sm:hidden">
        {rules.map((rule) => {
          const note = stateNote(rule);
          return (
            <li key={rule.id} className="grid gap-2 px-4 py-3 text-sm">
              <div className="flex items-start justify-between gap-2">
                {link(rule)}
                {menu(rule)}
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <OriginBadge origin={rule.origin} showName />
                <StateBadge state={ruleState(rule, pause)} />
                <span className="text-xs text-muted-foreground">v{rule.version}</span>
              </div>
              {note ? <p className="text-xs text-muted-foreground">{note}</p> : null}
              <p className="text-xs text-muted-foreground">
                {t("alerts.rules.applies_of", { applies: rule.applies_to.join(", ") })} · {t("alerts.rules.last_of")}{" "}
                {lastTrigger(rule)}
              </p>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function RuleMenu({
  rule,
  onDisable,
  onEnable,
  enabling,
}: {
  rule: RuleSummary;
  onDisable(rule: { id: string; name: string }): void;
  onEnable(id: string): void;
  enabling: boolean;
}) {
  const { t } = useI18n();
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          className="size-8"
          aria-label={t("alerts.rules.actions_of", { rule: rule.name })}
        >
          <EllipsisIcon aria-hidden="true" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        {rule.status === "active" ? (
          <DropdownMenuItem onSelect={() => onDisable({ id: rule.id, name: rule.name })}>
            <CircleOffIcon aria-hidden="true" />
            {t("alerts.rules.menu.disable")}
          </DropdownMenuItem>
        ) : null}
        {rule.status === "disabled" ? (
          <DropdownMenuItem disabled={enabling} onSelect={() => onEnable(rule.id)}>
            <PlayIcon aria-hidden="true" />
            {t("alerts.rules.menu.enable")}
          </DropdownMenuItem>
        ) : null}
        <DropdownMenuItem asChild>
          <Link to={rulePath(rule.id, { section: "versions" })}>{t("alerts.rules.menu.versions")}</Link>
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
