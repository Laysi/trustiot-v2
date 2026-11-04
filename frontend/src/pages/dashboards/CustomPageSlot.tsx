import type { DashboardVersion } from "../../api/dashboards.ts";
import { CustomPageFrame } from "../../dashboard/custom-page/CustomPageFrame.tsx";
import type { DashboardData } from "../../dashboard/data.ts";

export interface CustomPageSlotProps {
  dashboardId: string;
  /** The version shown: its `page` and `runtime` are the custom page and the runtime it runs with. */
  version: DashboardVersion;
  /** The version's datasets, computations and constants, and this viewing's results. */
  data: DashboardData;
  /** Runs one dataset again (the same as the components' retry). */
  onRetry(dataset: string): void;
}

/**
 * The place of the custom page in a version's component list (docs/architecture/ai-assistant.md,
 * 「自訂頁面」). `CustomPageFrame` draws the whole card and decides when the frame may be seen, so
 * this adds nothing around it. A version with a `custom_page` item always has a page and a runtime
 * (contract `validate_version`); without them nothing is drawn.
 */
export function CustomPageSlot({ version, data, onRetry }: CustomPageSlotProps) {
  // The API leaves both out or sends null for a version without a custom page.
  if (version.page === undefined || version.page === null) return null;
  if (version.runtime === undefined || version.runtime === null) return null;
  // The frame offers 「reload the data」 while a dataset has failed: run those datasets again.
  const onRefresh = () => {
    for (const [dataset, state] of Object.entries(data.results)) {
      if (state.status === "failed") onRetry(dataset);
    }
  };
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
