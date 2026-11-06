import { createContext, type ReactNode, useContext } from "react";

import { GatewaysAvailable } from "../../pages/gateways/available.tsx";
import { useSessionBound } from "../../session/PinSession.tsx";
import type { GatewaysApi } from "./gateways.ts";

const GatewaysApiContext = createContext<GatewaysApi | null>(null);

/**
 * Gives the pages the gateway endpoints: a fake in the UI preview and in tests. The app does not
 * mount it yet because the binding and health endpoints are pending (api/pending/gateways.ts);
 * the change that replaces them mounts a provider over the real client in `AppProviders`.
 */
export function GatewaysApiProvider({ api, children }: { api: GatewaysApi; children: ReactNode }) {
  return (
    <GatewaysApiContext.Provider value={api}>
      <GatewaysAvailable>{children}</GatewaysAvailable>
    </GatewaysApiContext.Provider>
  );
}

/** The gateway API, bound to the page: its requests name the page's user and tenant (`PinSession`). */
export function useGatewaysApi(): GatewaysApi {
  const value = useContext(GatewaysApiContext);
  if (value === null) {
    throw new Error("useGatewaysApi needs a GatewaysApiProvider");
  }
  return useSessionBound(value);
}
