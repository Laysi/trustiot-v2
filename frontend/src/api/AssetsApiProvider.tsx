import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../session/PinSession.tsx";
import type { AssetsApi } from "./assets.ts";

const AssetsApiContext = createContext<AssetsApi | null>(null);

/** Gives the pages the model and device write endpoints: `assetsApi` in the app, a fake server in tests. */
export function AssetsApiProvider({ api, children }: { api: AssetsApi; children: ReactNode }) {
  return <AssetsApiContext.Provider value={api}>{children}</AssetsApiContext.Provider>;
}

/** The assets API, bound to the page: its requests name the page's user and tenant (`PinSession`). */
export function useAssetsApi(): AssetsApi {
  const value = useContext(AssetsApiContext);
  if (value === null) {
    throw new Error("useAssetsApi needs an AssetsApiProvider");
  }
  return useSessionBound(value);
}
