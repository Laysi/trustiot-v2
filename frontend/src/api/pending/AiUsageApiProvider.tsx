// Pending Laysi/trustiot-v2#140: replace with a module over the generated client when api/openapi.json has these endpoints.
import { createContext, type ReactNode, useContext } from "react";

import { USAGE_SECTION_ID } from "../../pages/platform/ai/plugin.ts";
import { PlatformSectionsAvailable } from "../../pages/platform/available.tsx";
import { useSessionBound } from "../../session/PinSession.tsx";
import type { AiUsageApi } from "./aiUsage.ts";

const AiUsageApiContext = createContext<AiUsageApi | null>(null);
const SECTIONS: readonly string[] = [USAGE_SECTION_ID];

/** Gives the platform pages the usage endpoints: the real client once it exists, a fake in tests and the preview. */
export function AiUsageApiProvider({ api, children }: { api: AiUsageApi; children: ReactNode }) {
  return (
    <AiUsageApiContext.Provider value={api}>
      <PlatformSectionsAvailable ids={SECTIONS}>{children}</PlatformSectionsAvailable>
    </AiUsageApiContext.Provider>
  );
}

export function useAiUsageApi(): AiUsageApi {
  const value = useContext(AiUsageApiContext);
  if (value === null) {
    throw new Error("useAiUsageApi needs an AiUsageApiProvider");
  }
  return useSessionBound(value);
}
