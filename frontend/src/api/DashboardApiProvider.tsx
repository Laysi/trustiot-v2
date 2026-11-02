import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../session/PinSession.tsx";
import type { DashboardApi } from "./dashboards.ts";

const DashboardApiContext = createContext<DashboardApi | null>(null);

/** Gives the pages the assistant's dashboard endpoints: `dashboardApi` in the app, a fake server in tests. */
export function DashboardApiProvider({ api, children }: { api: DashboardApi; children: ReactNode }) {
  return <DashboardApiContext.Provider value={api}>{children}</DashboardApiContext.Provider>;
}

/** The dashboard API, bound to the page: its requests name the page's user and tenant (`PinSession`). */
export function useDashboardApi(): DashboardApi {
  const value = useContext(DashboardApiContext);
  if (value === null) {
    throw new Error("useDashboardApi needs a DashboardApiProvider");
  }
  return useSessionBound(value);
}
