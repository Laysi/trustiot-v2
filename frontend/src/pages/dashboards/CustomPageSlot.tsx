import { FileCode2Icon } from "lucide-react";
import { useId } from "react";

import type { DashboardVersion } from "../../api/dashboards.ts";
import { CustomPageFrame } from "../../dashboard/custom-page/CustomPageFrame.tsx";
import type { DashboardData } from "../../dashboard/data.ts";
import { useI18n } from "../../i18n/useI18n.ts";

export interface CustomPageSlotProps {
  /** The version shown: its `page` and `runtime` are the custom page and the runtime it runs with. */
  version: DashboardVersion;
  /** The version's datasets, computations and constants, and this viewing's results. */
  data: DashboardData;
  /** Runs the datasets again; the frame offers it when one failed. */
  onRefresh(): void;
}

/**
 * The place of the custom page in a version's component list (docs/architecture/ai-assistant.md,
 * 「自訂頁面」): the sandbox frame (`CustomPageFrame`), which draws its own card with the version's
 * title, the 「自訂圖表」 label, the source button and the frame. The frame shows the page only
 * while the sandbox runtime's report says it matches the data sent (frontend/sandbox/README.md,
 * 「顯示時比對的強制」).
 *
 * The API gives every version with a `custom_page` item its `page` and `runtime` (the contract's
 * `validate_version`), so a version without them is not expected; it gets a card saying the item
 * can't be shown rather than an empty place, and nothing is requested from `/sandbox/`.
 */
export function CustomPageSlot({ version, data, onRefresh }: CustomPageSlotProps) {
  if (version.page == null || version.runtime == null) return <CustomPageUnavailable />;
  return (
    <CustomPageFrame
      page={version.page}
      runtime={version.runtime}
      data={data}
      title={version.title}
      versionCreatedAt={version.created_at}
      onRefresh={onRefresh}
    />
  );
}

function CustomPageUnavailable() {
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
