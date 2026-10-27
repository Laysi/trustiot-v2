import { createContext, type ReactNode, useContext } from "react";

import type { IdentityApi } from "./identity.ts";

const IdentityApiContext = createContext<IdentityApi | null>(null);

/** Gives the pages the identity service's endpoints: `identityApi` in the app, a fake in tests. */
export function IdentityApiProvider({ api, children }: { api: IdentityApi; children: ReactNode }) {
  return <IdentityApiContext.Provider value={api}>{children}</IdentityApiContext.Provider>;
}

export function useIdentityApi(): IdentityApi {
  const value = useContext(IdentityApiContext);
  if (value === null) {
    throw new Error("useIdentityApi needs an IdentityApiProvider");
  }
  return value;
}
