import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RotateCcwIcon } from "lucide-react";
import { useNavigate } from "react-router";

import { useDashboardApi } from "../../api/DashboardApiProvider.tsx";
import { ApiError } from "../../api/errors.ts";
import { dashboardPath } from "../../app/paths.ts";
import { Button, type ButtonProps } from "../../components/ui/button.tsx";
import { useI18n } from "../../i18n/useI18n.ts";
import { useSession } from "../../session/SessionProvider.tsx";
import { ConfirmAction } from "./ConfirmAction.tsx";
import { dashboardListsKey, dashboardViewsKey, versionListKey } from "./queries.ts";

/**
 * Restores an older version as the dashboard's next one (`POST …/restore`) after asking, then
 * shows the latest version. The server refuses when a newer version came in since the page loaded
 * (the dashboard is read again then), when the dashboard is archived, and when the version equals
 * the latest; the dialog says which, translated, and stays open.
 */
export function RestoreAction(props: {
  dashboardId: string;
  /** The older version to restore. */
  version: number;
  /** The latest version the page knows: the request names it, so a newer one is a conflict. */
  latest: number;
  onRestored(created: { from: number; version: number }): void;
  variant?: ButtonProps["variant"];
}) {
  const { t } = useI18n();
  const api = useDashboardApi();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const { dashboardId, version: from, latest } = props;
  const refresh = () =>
    Promise.all([
      queryClient.invalidateQueries({ queryKey: dashboardViewsKey(tenant, dashboardId) }),
      queryClient.invalidateQueries({ queryKey: versionListKey(tenant, dashboardId) }),
      queryClient.invalidateQueries({ queryKey: dashboardListsKey(tenant) }),
    ]);
  const mutation = useMutation({
    mutationFn: () => api.restore(dashboardId, { from_version: from, base_version: latest }),
    onSuccess: async (created) => {
      props.onRestored({ from, version: created.version });
      await refresh();
      void navigate(dashboardPath(dashboardId));
    },
    onError: (error) => {
      // A newer version or an archive since the page loaded: show the dashboard as it is now.
      if (error instanceof ApiError && error.status === 409) void refresh();
    },
  });
  return (
    <ConfirmAction
      trigger={
        <Button size="sm" variant={props.variant ?? "default"}>
          <RotateCcwIcon />
          {t("dashboard_page.restore", { version: from })}
        </Button>
      }
      title={t("dashboard_page.restore_title", { version: from })}
      body={t("dashboard_page.restore_body", { version: from, next: latest + 1, latest })}
      confirm={t("dashboard_page.restore", { version: from })}
      pending={t("dashboard_page.restoring")}
      mutation={mutation}
    />
  );
}
