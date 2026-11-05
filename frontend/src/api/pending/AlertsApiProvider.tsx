// Pending Laysi/trustiot-v2#169: replace with a provider over the generated client when api/openapi.json has these endpoints.
import { createContext, type ReactNode, useContext } from "react";

import type { AlertsApi } from "./alerts.ts";

const AlertsApiContext = createContext<AlertsApi | null>(null);

/** Gives the alerts pages their API: the real one once it exists, a fake in the UI preview and in tests. */
export function AlertsApiProvider({ api, children }: { api: AlertsApi; children: ReactNode }) {
  return <AlertsApiContext.Provider value={api}>{children}</AlertsApiContext.Provider>;
}

export function useAlertsApi(): AlertsApi {
  const value = useContext(AlertsApiContext);
  if (value === null) {
    throw new Error("useAlertsApi needs an AlertsApiProvider");
  }
  return value;
}
