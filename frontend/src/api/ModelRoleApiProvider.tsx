import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../session/PinSession.tsx";
import type { ModelRoleApi } from "./modelRoles.ts";

const ModelRoleApiContext = createContext<ModelRoleApi | null>(null);

/** Gives the platform pages the model role endpoints: `modelRoleApi` in the app, a fake server in tests. */
export function ModelRoleApiProvider({ api, children }: { api: ModelRoleApi; children: ReactNode }) {
  return <ModelRoleApiContext.Provider value={api}>{children}</ModelRoleApiContext.Provider>;
}

/** The model role API, bound to the page (`PinSession`). */
export function useModelRoleApi(): ModelRoleApi {
  const value = useContext(ModelRoleApiContext);
  if (value === null) {
    throw new Error("useModelRoleApi needs a ModelRoleApiProvider");
  }
  return useSessionBound(value);
}
