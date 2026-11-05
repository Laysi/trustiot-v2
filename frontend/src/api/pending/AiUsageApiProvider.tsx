// Pending Laysi/trustiot-v2#140: replace with a module over the generated client when api/openapi.json has these endpoints.
import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../../session/PinSession.tsx";
import type { AiUsageApi } from "./aiUsage.ts";

const AiUsageApiContext = createContext<AiUsageApi | null>(null);

/** Gives the platform pages the usage endpoints: the real client once it exists, a fake in tests and the preview. */
export function AiUsageApiProvider({ api, children }: { api: AiUsageApi; children: ReactNode }) {
  return <AiUsageApiContext.Provider value={api}>{children}</AiUsageApiContext.Provider>;
}

export function useAiUsageApi(): AiUsageApi {
  const value = useContext(AiUsageApiContext);
  if (value === null) {
    throw new Error("useAiUsageApi needs an AiUsageApiProvider");
  }
  return useSessionBound(value);
}
