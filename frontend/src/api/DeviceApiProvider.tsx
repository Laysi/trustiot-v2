import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../session/PinSession.tsx";
import type { DeviceApi } from "./devices.ts";

const DeviceApiContext = createContext<DeviceApi | null>(null);

/** Gives the pages the device and telemetry endpoints: `deviceApi` in the app, a fake server in tests. */
export function DeviceApiProvider({ api, children }: { api: DeviceApi; children: ReactNode }) {
  return <DeviceApiContext.Provider value={api}>{children}</DeviceApiContext.Provider>;
}

/** The device API, bound to the page: its requests name the page's user and tenant (`PinSession`). */
export function useDeviceApi(): DeviceApi {
  const value = useContext(DeviceApiContext);
  if (value === null) {
    throw new Error("useDeviceApi needs a DeviceApiProvider");
  }
  return useSessionBound(value);
}
