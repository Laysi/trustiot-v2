import { createContext, type ReactNode, useContext } from "react";

import { MODEL_ROLES_SECTION_ID } from "../pages/platform/ai/plugin.ts";
import { PlatformSectionsAvailable } from "../pages/platform/available.tsx";
import { useSessionBound } from "../session/PinSession.tsx";
import type { ModelRoleApi } from "./modelRoles.ts";

const ModelRoleApiContext = createContext<ModelRoleApi | null>(null);
const SECTIONS: readonly string[] = [MODEL_ROLES_SECTION_ID];

/**
 * Gives the platform pages the model role endpoints: `modelRoleApi` in the app, a fake server in
 * tests. It also puts the model roles in the platform layout's navigation (`available.tsx`).
 */
export function ModelRoleApiProvider({ api, children }: { api: ModelRoleApi; children: ReactNode }) {
  return (
    <ModelRoleApiContext.Provider value={api}>
      <PlatformSectionsAvailable ids={SECTIONS}>{children}</PlatformSectionsAvailable>
    </ModelRoleApiContext.Provider>
  );
}

/** The model role API, bound to the page (`PinSession`). */
export function useModelRoleApi(): ModelRoleApi {
  const value = useContext(ModelRoleApiContext);
  if (value === null) {
    throw new Error("useModelRoleApi needs a ModelRoleApiProvider");
  }
  return useSessionBound(value);
}
