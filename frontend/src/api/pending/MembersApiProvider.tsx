// Pending Laysi/trustiot-v2#105: replace with a provider over the generated client when api/openapi.json has the member endpoints.
import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../../session/PinSession.tsx";
import type { MembersApi } from "./members.ts";

const MembersApiContext = createContext<MembersApi | null>(null);

/** Gives the member pages their API: a fake in the UI preview and in tests, nothing in production. */
export function MembersApiProvider({ api, children }: { api: MembersApi; children: ReactNode }) {
  return <MembersApiContext.Provider value={api}>{children}</MembersApiContext.Provider>;
}

/** The member API, bound to the page like the other APIs (`PinSession`). */
export function useMembersApi(): MembersApi {
  const value = useContext(MembersApiContext);
  if (value === null) {
    throw new Error("useMembersApi needs a MembersApiProvider");
  }
  return useSessionBound(value);
}
