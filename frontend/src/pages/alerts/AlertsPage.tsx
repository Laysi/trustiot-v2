import { useQuery } from "@tanstack/react-query";
import { LockIcon, SearchXIcon } from "lucide-react";
import { useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import { ApiError } from "../../api/errors.ts";
import { useAlertsApi } from "../../api/pending/AlertsApiProvider.tsx";
import { Alert, AlertDescription } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { StatusPage } from "../StatusPage.tsx";
import { AiRuleStrip } from "./AiRuleStrip.tsx";
import { DisableRuleDialog } from "./DisableRuleDialog.tsx";
import { NotificationsTab } from "./NotificationsTab.tsx";
import { ActionError, AskAiButton, ListSkeleton, Tabs } from "./parts.tsx";
import { type AlertsTab, alertsPath } from "./paths.ts";
import { notificationKey, overviewKey } from "./queries.ts";
import { RulesTab } from "./RulesTab.tsx";
import { useAlertsRole, useTenantId } from "./state.ts";

const TAB_PARAM = "tab";
const TAB_ID_PREFIX = "alerts";

/**
 * The alerts and notification center (`/alerts`, `/alerts/notifications/:notificationId`, design
 * docs/design/ms4-ms5/design.md 「警報與通知中心」): the state of the AI rules above two tabs,
 * the notifications and the rules. The tab is in the address (`?tab=rules`). A tenant whose plan
 * has no alerts sees the upgrade wall instead (plans.md); a failed load says what keeps working.
 */
export function AlertsPage() {
  const { t, errorMessage, formatNumber } = useI18n();
  const api = useAlertsApi();
  const tenant = useTenantId();
  const { notificationId } = useParams();
  const [params, setParams] = useSearchParams();
  const navigate = useNavigate();
  const [disableTarget, setDisableTarget] = useState<{ id: string; name: string } | null>(null);
  const [actionError, setActionError] = useState<unknown>(null);
  const { canManage } = useAlertsRole();

  const overview = useQuery({
    queryKey: overviewKey(tenant),
    queryFn: ({ signal }) => api.getOverview(signal),
  });
  // A deep link names one notification; one that does not exist is the page's "not found".
  const linked = useQuery({
    queryKey: notificationKey(tenant, notificationId ?? ""),
    queryFn: ({ signal }) => api.getNotification(notificationId ?? "", signal),
    enabled: notificationId !== undefined,
    retry: false,
  });

  const tab: AlertsTab = notificationId !== undefined || params.get(TAB_PARAM) !== "rules" ? "notifications" : "rules";
  const selectTab = (next: AlertsTab) => {
    // On a notification's deep link the other tab is the plain page with that tab, so the link's notification stays out of it.
    if (notificationId !== undefined) {
      if (next !== "notifications") void navigate(alertsPath(next), { replace: true });
      return;
    }
    setParams(
      (current) => {
        const query = new URLSearchParams(current);
        if (next === "rules") query.set(TAB_PARAM, "rules");
        else query.delete(TAB_PARAM);
        return query;
      },
      { replace: true },
    );
  };

  const header = (
    <PageHeader
      title={t("alerts.title")}
      meta={t("alerts.description")}
      actions={
        overview.data?.plan_allows === false ? null : (
          <AskAiButton
            question={t("alerts.ask.suggest_rules")}
            context={[t("alerts.ask.context_all_devices"), t("alerts.ask.context_14_days")]}
          />
        )
      }
    />
  );

  if (overview.isPending) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={t("alerts.title")} />
        <ListSkeleton label={t("alerts.notifications.loading")} />
      </div>
    );
  }
  if (overview.isError) {
    return (
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
        <PageHeader title={t("alerts.title")} />
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p className="font-medium">{t("alerts.error.title")}</p>
            <p>{t("alerts.error.body")}</p>
            <p className="text-xs">{errorMessage(overview.error)}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void overview.refetch()} disabled={overview.isFetching}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      </div>
    );
  }
  if (!overview.data.plan_allows) return <UpgradeWall />;

  if (
    notificationId !== undefined &&
    linked.isError &&
    linked.error instanceof ApiError &&
    linked.error.status === 404
  ) {
    return (
      <StatusPage
        title={t("alerts.notification_missing.title")}
        icon={<SearchXIcon />}
        actions={
          <Button asChild variant="outline">
            <Link to={alertsPath()}>{t("alerts.notification_missing.back")}</Link>
          </Button>
        }
      >
        <p>{t("alerts.notification_missing.body")}</p>
      </StatusPage>
    );
  }

  const data = overview.data;
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      {header}
      <AiRuleStrip overview={data} canManage={canManage} onError={setActionError} />
      {actionError !== null ? (
        <ActionError message={errorMessage(actionError)} onDismiss={() => setActionError(null)} />
      ) : null}
      {notificationId !== undefined && linked.isError ? (
        <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center gap-x-3">
          <AlertDescription className="col-start-1">
            <p>{t("alerts.notification_missing.load_failed", { reason: errorMessage(linked.error) })}</p>
          </AlertDescription>
          <Button variant="outline" onClick={() => void linked.refetch()} disabled={linked.isFetching}>
            {t("page.action.retry")}
          </Button>
        </Alert>
      ) : null}
      <Tabs
        label={t("alerts.tabs_label")}
        idPrefix={TAB_ID_PREFIX}
        value={tab}
        onChange={selectTab}
        tabs={[
          {
            id: "notifications",
            label: t("alerts.tab.notifications"),
            count:
              data.unread_count > 0
                ? {
                    text: formatNumber(data.unread_count),
                    label: t("alerts.tab.unread", { count: formatNumber(data.unread_count) }),
                  }
                : undefined,
          },
          {
            id: "rules",
            label: t("alerts.tab.rules"),
            count: {
              text: formatNumber(data.rule_count),
              label: t("alerts.tab.rule_count", { count: formatNumber(data.rule_count) }),
            },
          },
        ]}
      />
      <div role="tabpanel" id={`${TAB_ID_PREFIX}-panel-${tab}`} aria-labelledby={`${TAB_ID_PREFIX}-tab-${tab}`}>
        {tab === "notifications" ? (
          <NotificationsTab
            unreadCount={data.unread_count}
            totalCount={data.notification_count}
            highlight={linked.data}
            onDisable={setDisableTarget}
            onError={setActionError}
          />
        ) : (
          <RulesTab pause={data.pause} onDisable={setDisableTarget} onError={setActionError} />
        )}
      </div>
      <DisableRuleDialog
        rule={disableTarget}
        onOpenChange={(open) => {
          if (!open) setDisableTarget(null);
        }}
      />
    </div>
  );
}

/** The plan has no alerts (docs/product/plans.md): what they do, and the next step for each role. */
function UpgradeWall() {
  const { t } = useI18n();
  const { isAdmin } = useAlertsRole();
  return (
    <div className="grid grid-cols-[minmax(0,1fr)] gap-4">
      <PageHeader title={t("alerts.title")} />
      <section className="grid justify-items-center gap-3 rounded-lg border border-dashed px-4 py-12 text-center">
        <LockIcon aria-hidden="true" className="size-6 text-muted-foreground" />
        <h2 className="text-lg font-semibold">{t("alerts.starter.title")}</h2>
        <p className="max-w-prose text-sm text-muted-foreground">{t("alerts.starter.body")}</p>
        <AskAiButton
          question={t("alerts.starter.ask")}
          context={[t("alerts.ask.context_all_devices"), t("alerts.ask.context_14_days")]}
        />
        <p className="text-xs text-muted-foreground">
          {t(isAdmin ? "alerts.starter.note_admin" : "alerts.starter.note_member")}
        </p>
      </section>
    </div>
  );
}
