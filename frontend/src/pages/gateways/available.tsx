import { createContext, type ReactNode, useContext } from "react";

const GatewaysAvailableContext = createContext(false);

/**
 * Says to the pages below that the build has the gateway page. Whoever provides the gateway API
 * mounts it (api/pending/GatewaysApiProvider.tsx in the UI preview; the provider over the real
 * client once the endpoints exist). It lives here, not next to the pending API, because the
 * product's pages read it: the build must not carry `src/api/pending/` (`check:dist`).
 */
export function GatewaysAvailable({ children }: { children: ReactNode }) {
  return <GatewaysAvailableContext.Provider value={true}>{children}</GatewaysAvailableContext.Provider>;
}

/**
 * Whether the gateway page exists in this build. Other pages (the empty device list's first step)
 * link to it only then, so a link never leads to a page the build does not have.
 */
export function useGatewaysAvailable(): boolean {
  return useContext(GatewaysAvailableContext);
}
