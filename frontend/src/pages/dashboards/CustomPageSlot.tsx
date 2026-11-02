import { FileCode2Icon } from "lucide-react";
import { useId } from "react";

import type { DashboardVersion } from "../../api/dashboards.ts";
import type { DashboardData } from "../../dashboard/data.ts";
import { useI18n } from "../../i18n/useI18n.ts";

export interface CustomPageSlotProps {
  dashboardId: string;
  /** The version shown: its `page` and `runtime` are the custom page and the runtime it runs with. */
  version: DashboardVersion;
  /** The version's datasets, computations and constants, and this viewing's results. */
  data: DashboardData;
}

/**
 * The place of the custom page in a version's component list (docs/architecture/ai-assistant.md,
 * 「自訂頁面」). Until the sandbox's outer frame lands (Laysi/trustiot-v2#997) it is a placeholder
 * that says the item can't be shown: no iframe, no request to `/sandbox/`, none of the page's code
 * runs. Laysi/trustiot-v2#997 replaces the body of this component with the sandbox frame; the
 * props are what that frame takes, so the page does not change.
 */
export function CustomPageSlot(_props: CustomPageSlotProps) {
  const { t } = useI18n();
  const titleId = useId();
  return (
    <section
      aria-labelledby={titleId}
      data-component="custom_page"
      className="grid justify-items-start gap-2 rounded-xl border border-dashed bg-card px-4 py-5 text-card-foreground sm:px-5"
    >
      <h3
        id={titleId}
        className="flex items-center gap-2 text-base font-semibold [&_svg]:size-4 [&_svg]:text-muted-foreground"
      >
        <FileCode2Icon aria-hidden="true" />
        {t("dashboard_page.custom_page.title")}
      </h3>
      <p role="status" className="text-sm text-muted-foreground text-pretty">
        {t("dashboard_page.custom_page.unavailable")}
      </p>
    </section>
  );
}
