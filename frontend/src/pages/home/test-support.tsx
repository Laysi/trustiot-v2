import type { ReactNode } from "react";
import type { RouteObject } from "react-router";

import { type HomeOverviewApi, HomeOverviewApiProvider } from "../../api/pending/home-overview.ts";
import { type SetupApi, SetupApiProvider } from "../../api/pending/setup.ts";
import { Layout } from "../../app/Layout.tsx";
import { TenantGate } from "../../session/TenantGate.tsx";
import { HomePage } from "../pages.tsx";
import { HomeTopSlotProvider } from "./HomeSlot.tsx";
import { OverviewPage } from "./OverviewPage.tsx";
import { SetupChecklist } from "./SetupChecklist.tsx";

/**
 * The routes the home tests render: the home page and the generic home under the tenant gate, with
 * the pending APIs provided as the UI preview provides them, plus plain pages at `pages` (the
 * routes a test says exist, so the steps' links can be told from the steps without one).
 */
export function homeRoutes(options: { setup?: SetupApi; overview?: HomeOverviewApi; pages?: string[] }): RouteObject[] {
  const provide = (node: ReactNode) => {
    let wrapped = node;
    if (options.setup) {
      wrapped = (
        <SetupApiProvider api={options.setup}>
          <HomeTopSlotProvider slot={<SetupChecklist />}>{wrapped}</HomeTopSlotProvider>
        </SetupApiProvider>
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
            ...(options.pages ?? []).map((path) => ({ path: path.slice(1), element: <p>{path}</p> })),
          ],
        },
      ],
    },
  ];
}
