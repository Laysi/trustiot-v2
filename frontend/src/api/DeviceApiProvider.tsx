import { createContext, type ReactNode, useContext } from "react";

import type { DeviceApi } from "./devices.ts";

const DeviceApiContext = createContext<DeviceApi | null>(null);

/** Gives the pages the device and telemetry endpoints: `deviceApi` in the app, a fake server in tests. */
export function DeviceApiProvider({ api, children }: { api: DeviceApi; children: ReactNode }) {
  return <DeviceApiContext.Provider value={api}>{children}</DeviceApiContext.Provider>;
}

export function useDeviceApi(): DeviceApi {
  const value = useContext(DeviceApiContext);
  if (value === null) {
    throw new Error("useDeviceApi needs a DeviceApiProvider");
  }
  return value;
}
