// Pending Laysi/trustiot-v2#177: replace with a provider over the generated client when api/openapi.json has the machine endpoints.
import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../../session/PinSession.tsx";
import type { MachineApi } from "./machines.ts";

const MachineApiContext = createContext<MachineApi | null>(null);

/** Gives the machine pages the field-context endpoints: a fake in the UI preview and in tests. */
export function MachineApiProvider({ api, children }: { api: MachineApi; children: ReactNode }) {
  return <MachineApiContext.Provider value={api}>{children}</MachineApiContext.Provider>;
}

/** The machine API, bound to the page like the other APIs (`PinSession`). */
export function useMachineApi(): MachineApi {
  const value = useContext(MachineApiContext);
  if (value === null) {
    throw new Error("useMachineApi needs a MachineApiProvider");
  }
  return useSessionBound(value);
}
