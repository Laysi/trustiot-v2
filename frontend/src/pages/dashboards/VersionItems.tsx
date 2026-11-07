import { useId, useMemo } from "react";

import type { DashboardVersion, ShownItem } from "../../api/dashboards.ts";
import { DashboardComponent } from "../../dashboard/DashboardComponent.tsx";
import type { DashboardData } from "../../dashboard/data.ts";
import { isPlatformComponent } from "../../dashboard/spec.ts";
import { componentDatasets } from "../../dashboard/uses.ts";
import { useI18n } from "../../i18n/useI18n.ts";
import { cn } from "../../lib/utils.ts";
import { CustomPageSlot } from "./CustomPageSlot.tsx";
import { type DatasetRuns, useDatasetRuns } from "./useDatasetRuns.ts";

// The drawing of one version's component list, shared by the dashboard page and the conversation's
// draft panel, so a draft shows exactly what its dashboard will show once published.

/** A version's component list; a version written before 2026-11-01 has only its page (`Spec::layout`). */
export function layoutOf(version: DashboardVersion): ShownItem[] {
  const components = version.spec.components ?? [];
  if (components.length > 0) return components;
  return version.page != null ? [{ kind: "custom_page" }] : [];
}

export interface VersionContent {
  /** What the version shows, in order. */
  items: ShownItem[];
  /** The version's datasets, computations and constants, and this viewing's results. */
  data: DashboardData;
  runs: DatasetRuns;
}

/**
 * Runs the datasets of one version as the viewer, the shown components' first and then the rest
 * (a custom page's), and gives what the components draw from. `dashboardId` is the dashboard the
 * version belongs to: a draft's own ID for a draft (the run endpoint takes a draft too).
 */
export function useVersionContent(
  dashboardId: string,
  version: DashboardVersion,
  /** Whether the datasets run yet (see `useDatasetRuns`); a view that is far from the viewport waits. */
  enabled = true,
): VersionContent {
  const items = useMemo(() => layoutOf(version), [version]);
  const datasets = useMemo(() => {
    const computations = version.spec.computations ?? [];
    const shown = items.flatMap((item) =>
      item.kind !== "unsupported" && isPlatformComponent(item) ? componentDatasets(item, { computations }) : [],
    );
    const declared = version.spec.datasets.map((dataset) => dataset.name);
    return [...new Set([...shown.filter((name) => declared.includes(name)), ...declared])];
  }, [items, version]);
  const runs = useDatasetRuns(dashboardId, version.version, datasets, enabled);
  const data = useMemo<DashboardData>(
    () => ({
      datasets: version.spec.datasets,
      computations: version.spec.computations ?? [],
      constants: version.spec.constants ?? [],
      results: runs.results,
    }),
    [version, runs.results],
  );
  return { items, data, runs };
}

/**
 * A KPI and a status sit side by side; charts, tables and pages take the whole width. While a KPI
 * or a status has its source panel open (`data-source-panel`, drawn inside the component), it takes
 * the whole row too, so the panel is as wide as the grid instead of one cell (Laysi/trustiot-v2#1197):
 * the items before it keep their places and the ones after it move below it, in reading order.
 */
function itemClass(item: ShownItem, wide: string): string {
  return item.kind === "kpi" || item.kind === "status"
    ? "min-w-0 has-[[data-source-panel]]:col-span-full"
    : cn("min-w-0", wide);
}

/**
 * The items of a version, one component each. `layout` is where it is drawn: the whole page
 * (up to four cards a row), a side panel (up to two), or an answer in a conversation: two when
 * the answer's own width has room for them, whatever the window's width is (a container query).
 */
export function VersionItems(props: {
  version: DashboardVersion;
  content: VersionContent;
  label: string;
  layout: "page" | "panel" | "answer";
}) {
  const { t } = useI18n();
  const { version, content, layout } = props;
  const { items, data, runs } = content;
  const page = layout === "page";
  const answer = layout === "answer";
  const wide = page ? "sm:col-span-2 xl:col-span-4" : answer ? "@lg:col-span-2" : "sm:col-span-2";
  return (
    <div className={cn(answer && "@container")}>
      <section
        aria-label={props.label}
        aria-busy={runs.refreshing}
        // The earlier content stays while the datasets run again, dimmed (opacity-refetch in the
        // design system); the page never goes back to skeletons.
        className={cn(
          "grid gap-4 transition-opacity duration-150",
          answer ? "@lg:grid-cols-2" : "sm:grid-cols-2",
          page && "xl:grid-cols-4",
          runs.refreshing && "opacity-60",
        )}
      >
        {items.length === 0 ? (
          <p className={cn("text-sm text-muted-foreground", wide)}>{t("dashboard_page.no_items")}</p>
        ) : null}
        {items.map((item, index) => (
          // The list is a version's, fixed for the life of this view: its index is its identity.
          // biome-ignore lint/suspicious/noArrayIndexKey: the items of a version never move
          <div key={index} className={itemClass(item, wide)}>
            {item.kind === "unsupported" ? (
              <UnsupportedItem />
            ) : isPlatformComponent(item) ? (
              <DashboardComponent spec={item} data={data} onRetry={runs.retry} />
            ) : (
              <CustomPageSlot version={version} data={data} onRefresh={runs.refresh} />
            )}
          </div>
        ))}
      </section>
    </div>
  );
}

/** An item of a newer kind, or with a setting, this version of the app does not know. */
function UnsupportedItem() {
  const { t } = useI18n();
  const titleId = useId();
  return (
    <section
      aria-labelledby={titleId}
      data-component="unsupported"
      className="grid justify-items-start gap-2 rounded-xl border border-dashed bg-card px-4 py-5 text-card-foreground sm:px-5"
    >
      <h3 id={titleId} className="text-base font-semibold">
        {t("dashboard_page.unsupported.title")}
      </h3>
      <p className="text-sm text-muted-foreground text-pretty">{t("dashboard_page.unsupported.body")}</p>
    </section>
  );
}
