import { useQueryClient } from "@tanstack/react-query";
import { CloudOffIcon, HourglassIcon, RefreshCwIcon, RouterIcon, SparklesIcon } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router";

import { EmptyState } from "../../components/EmptyState.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../components/ui/alert.tsx";
import { Button } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { tenantName } from "../../session/tenant-name.ts";
import { PageHeader } from "../dashboards/PageHeader.tsx";
import { DeviceTable, NewAnomaliesCard, StatusTiles, TopUsageCard } from "./OverviewBlocks.tsx";
import { overviewKeys, useDeviceHealth, useNewAnomalies, useStatusSummary, useTopUsage } from "./overview-queries.ts";
import { SetupChecklist } from "./SetupChecklist.tsx";
import { STEP_TARGETS } from "./setup-steps.ts";
import { useRouteExists } from "./useRouteExists.ts";

/**
 * The generic home (Laysi/trustiot-v2#129, docs/design/home-ai/design.md 「通用首頁」): the setup
 * checklist, the device status counts, the devices that used the most power today, the anomalies
 * since the last visit and the device table, with no composer. It is the layout the home falls back
 * to when the AI is not available. `aiPaused` is for the case the design 10 draws, an assistant
 * that is usually there but could not be reached when the page loaded: it adds the notice that the
 * conversation comes back when it recovers. Where there is no AI at all the notice is left out.
 */
export function GenericHome({ aiPaused = false }: { aiPaused?: boolean }) {
  const { t, formatDateTime } = useI18n();
  const { session } = useSession();
  const tenant = session?.activeTenant;
  const admin = tenant?.role === "admin";
  const queryClient = useQueryClient();
  const routeExists = useRouteExists();

  const summary = useStatusSummary();
  const usage = useTopUsage();
  const anomalies = useNewAnomalies();
  const devices = useDeviceHealth();

  const bindLink = routeExists(STEP_TARGETS.bind_gateway ?? "") ? STEP_TARGETS.bind_gateway : null;
  const addLink = routeExists(STEP_TARGETS.add_device ?? "") ? STEP_TARGETS.add_device : null;
  const deviceLink = routeExists(STEP_TARGETS.first_data ?? "") ? STEP_TARGETS.first_data : null;

  const queries = [summary, usage, anomalies, devices];
  const refreshing = queries.some((query) => query.isFetching);
  // Nothing at all could be read: the page says so once, instead of four blocks that each fail.
  const unreachable = queries.every((query) => query.isError && query.data === undefined);

  let body: ReactNode;
  if (unreachable) {
    body = (
      <EmptyState
        role="alert"
        icon={<CloudOffIcon />}
        title={t("overview.unreachable.title")}
        action={
          <Button
            size="sm"
            onClick={() => void queryClient.invalidateQueries({ queryKey: overviewKeys(tenant?.id ?? "") })}
          >
            <RefreshCwIcon aria-hidden="true" />
            {t("page.action.retry")}
          </Button>
        }
      >
        <p>{t("overview.unreachable.body")}</p>
      </EmptyState>
    );
  } else if (summary.data && summary.data.gateway_count === 0) {
    body = (
      <EmptyState
        icon={<RouterIcon />}
        title={t("overview.no_gateway.title")}
        action={
          admin && bindLink ? (
            <Button asChild size="sm">
              <Link to={bindLink}>{t("setup.step.bind_gateway.title")}</Link>
            </Button>
          ) : null
        }
      >
        <p>{admin ? t("overview.no_gateway.body") : t("overview.no_gateway.body_member")}</p>
      </EmptyState>
    );
  } else if (
    devices.data &&
    devices.data.length > 0 &&
    devices.data.every((device) => device.last_report_at === null)
  ) {
    body = (
      <EmptyState
        icon={<HourglassIcon />}
        title={t("overview.waiting.title")}
        action={
          deviceLink ? (
            <Button asChild variant="outline" size="sm">
              <Link to={deviceLink}>{t("setup.step.first_data.cta")}</Link>
            </Button>
          ) : null
        }
      >
        <p>{t("overview.waiting.body", { count: devices.data.length })}</p>
      </EmptyState>
    );
  } else {
    body = (
      <>
        <section aria-labelledby="overview-status" className="grid gap-3">
          <h2 id="overview-status" className="sr-only">
            {t("overview.status.title")}
          </h2>
          <StatusTiles query={summary} />
        </section>
        <div className="grid items-start gap-4 lg:grid-cols-2">
          <TopUsageCard query={usage} />
          <NewAnomaliesCard query={anomalies} />
        </div>
        <DeviceTable query={devices} canAddDevices={admin === true} addDeviceLink={addLink} />
      </>
    );
  }

  return (
    <div className="mx-auto grid w-full max-w-6xl gap-4">
      {aiPaused ? (
        <Alert role="status" className="grid-cols-[1rem_1fr]">
          <SparklesIcon aria-hidden="true" />
          <AlertTitle>{t("overview.ai_paused.title")}</AlertTitle>
          <AlertDescription>
            <p>{t("overview.ai_paused.body")}</p>
          </AlertDescription>
        </Alert>
      ) : null}
      <SetupChecklist />
      <PageHeader
        title={t("overview.title")}
        meta={[
          tenant ? tenantName(tenant, t) : null,
          summary.data ? t("overview.updated", { time: formatDateTime(summary.data.updated_at) }) : null,
        ]
          .filter((part) => part !== null)
          .join(" · ")}
        actions={
          <Button
            variant="outline"
            size="sm"
            disabled={refreshing}
            onClick={() => void queryClient.invalidateQueries({ queryKey: overviewKeys(tenant?.id ?? "") })}
          >
            <RefreshCwIcon aria-hidden="true" />
            {t("overview.refresh")}
          </Button>
        }
      />
      {body}
    </div>
  );
}
