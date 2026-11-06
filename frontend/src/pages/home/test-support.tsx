import type { ReactNode } from "react";
import type { RouteObject } from "react-router";

import { type HomeOverviewApi, HomeOverviewApiProvider } from "../../api/pending/home-overview.ts";
import { ReportApiProvider } from "../../api/pending/ReportApiProvider.tsx";
import type { ReportApi } from "../../api/pending/reports.ts";
import { type SetupApi, SetupApiProvider } from "../../api/pending/setup.ts";
import { Layout } from "../../app/Layout.tsx";
import { TenantGate } from "../../session/TenantGate.tsx";
import { HomePage } from "../pages.tsx";
import { ScheduleFormPage } from "../reports/ScheduleFormPage.tsx";
import { WeeklyReportStep } from "../reports/WeeklyReportStep.tsx";
import { FirstUseSlotProvider } from "./first-use-slot.tsx";
import { HomeTopSlotProvider } from "./HomeSlot.tsx";
import { OverviewPage } from "./OverviewPage.tsx";
import { SetupChecklist } from "./SetupChecklist.tsx";

/**
 * The routes the home tests render: the home page and the generic home under the tenant gate, with
 * the pending APIs provided as the UI preview provides them, plus plain pages at `pages` (the
 * routes a test says exist, so the steps' links can be told from the steps without one).
 */
export function homeRoutes(options: {
  setup?: SetupApi;
  overview?: HomeOverviewApi;
  /** The report API of the UI preview: the weekly report in the first-use step, and the form it leads to. */
  reports?: ReportApi;
  pages?: string[];
}): RouteObject[] {
  const provide = (node: ReactNode) => {
    let wrapped = node;
    if (options.setup) {
      wrapped = (
        <SetupApiProvider api={options.setup}>
          <HomeTopSlotProvider slot={<SetupChecklist />}>{wrapped}</HomeTopSlotProvider>
        </SetupApiProvider>
      );
    }
    if (options.reports) {
      wrapped = (
        <ReportApiProvider api={options.reports}>
          <FirstUseSlotProvider slot={<WeeklyReportStep />}>{wrapped}</FirstUseSlotProvider>
        </ReportApiProvider>
      );
    }
    if (options.overview) wrapped = <HomeOverviewApiProvider api={options.overview}>{wrapped}</HomeOverviewApiProvider>;
    return wrapped;
  };
  return [
    {
      path: "/",
      element: <Layout />,
      children: [
        {
          element: <TenantGate />,
          children: [
            { index: true, element: provide(<HomePage />) },
            { path: "overview", element: provide(<OverviewPage />) },
            ...(options.reports
              ? [
                  {
                    path: "reports/schedules/new",
                    element: (
                      <ReportApiProvider api={options.reports}>
                        <ScheduleFormPage />
                      </ReportApiProvider>
                    ),
                  },
                ]
              : []),
            ...(options.pages ?? []).map((path) => ({ path: path.slice(1), element: <p>{path}</p> })),
          ],
        },
      ],
    },
  ];
}
