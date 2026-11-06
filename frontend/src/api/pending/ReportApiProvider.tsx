// Pending Laysi/trustiot-v2#173: when the real client lands, `reportApi` joins `AppDependencies` like the other API providers.
import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../../session/PinSession.tsx";
import type { ReportApi } from "./reports.ts";

const ReportApiContext = createContext<ReportApi | null>(null);

/** Gives the reports pages the report endpoints: a fake server in the UI preview and in tests. */
export function ReportApiProvider({ api, children }: { api: ReportApi; children: ReactNode }) {
  return <ReportApiContext.Provider value={api}>{children}</ReportApiContext.Provider>;
}

/** The report API, bound to the page like the other APIs (`useSessionBound`). */
export function useReportApi(): ReportApi {
  const value = useContext(ReportApiContext);
  if (value === null) {
    throw new Error("useReportApi needs a ReportApiProvider");
  }
  return useSessionBound(value);
}
