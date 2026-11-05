// Pending Laysi/trustiot-v2#153: replace with a module over the generated client when api/openapi.json has these endpoints.
import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../../session/PinSession.tsx";
import type { PromptApi } from "./prompts.ts";

const PromptApiContext = createContext<PromptApi | null>(null);

/** Gives the platform pages the prompt endpoints: the real client once it exists, a fake in tests and the preview. */
export function PromptApiProvider({ api, children }: { api: PromptApi; children: ReactNode }) {
  return <PromptApiContext.Provider value={api}>{children}</PromptApiContext.Provider>;
}

export function usePromptApi(): PromptApi {
  const value = useContext(PromptApiContext);
  if (value === null) {
    throw new Error("usePromptApi needs a PromptApiProvider");
  }
  return useSessionBound(value);
}
