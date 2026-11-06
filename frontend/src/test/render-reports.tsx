import type { ReactElement } from "react";
import type { RouteObject } from "react-router";

import { ReportApiProvider } from "../api/pending/ReportApiProvider.tsx";
import { ROUTES } from "../app/routes.tsx";
import { type FileSaver, FileSaverContext } from "../pages/reports/download.ts";
import { ReportPage } from "../pages/reports/ReportPage.tsx";
import { ReportsPage } from "../pages/reports/ReportsPage.tsx";
import { ScheduleFormPage } from "../pages/reports/ScheduleFormPage.tsx";
import { withRoutes } from "../ui-preview/collect.ts";
import type { FakeReportServer } from "./fake-reports.ts";
import { renderAt } from "./render.tsx";

/** What a test saved instead of the browser: the name and the content's type and text. */
export interface SavedFile {
  name: string;
  type: string;
  text: string;
}

export function recordingSaver(): FileSaver & { saved: SavedFile[] } {
  const saved: SavedFile[] = [];
  return {
    saved,
    save(blob, name) {
      void blob.text().then((text) => saved.push({ name, type: blob.type, text }));
    },
  };
}

/**
 * The app at `path` with the reports pages under the tenant's pages (as the UI preview adds them)
 * and `server` as the report API, which the preview gets from a provider around the app.
 */
export function renderReports(
  path: string,
  server: FakeReportServer,
  options: Omit<NonNullable<Parameters<typeof renderAt>[1]>, "routes"> & { saver?: FileSaver } = {},
) {
  const { saver, ...rest } = options;
  const wrap = (page: ReactElement) => (
    <ReportApiProvider api={server.api()}>
      <FileSaverContext.Provider value={saver ?? recordingSaver()}>{page}</FileSaverContext.Provider>
    </ReportApiProvider>
  );
  const pages: RouteObject[] = [
    { path: "reports", element: wrap(<ReportsPage />) },
    { path: "reports/schedules/new", element: wrap(<ScheduleFormPage />) },
    { path: "reports/schedules/:scheduleId/edit", element: wrap(<ScheduleFormPage />) },
    { path: "reports/:reportId", element: wrap(<ReportPage />) },
  ];
  return renderAt(path, { ...rest, routes: withRoutes(ROUTES, pages) });
}
