import { createContext, type ReactNode, useContext } from "react";

import { PlatformSectionsAvailable } from "../../pages/platform/available.tsx";
import { PLATFORM_API_SECTION_IDS } from "../../pages/platform/sections.ts";
import { useSessionBound } from "../../session/PinSession.tsx";
import type { PlatformApi } from "./platform.ts";

const PlatformApiContext = createContext<PlatformApi | null>(null);

/**
 * Gives the platform back office's pages their API: a fake while the endpoints are pending. It
 * also puts their sections in the platform layout's navigation (`pages/platform/available.tsx`).
 */
export function PlatformApiProvider({ api, children }: { api: PlatformApi; children: ReactNode }) {
  return (
    <PlatformApiContext.Provider value={api}>
      <PlatformSectionsAvailable ids={PLATFORM_API_SECTION_IDS}>{children}</PlatformSectionsAvailable>
    </PlatformApiContext.Provider>
  );
}

/** The platform API, bound to the page: its requests name the page's user (`PinSession`). */
export function usePlatformApi(): PlatformApi {
  const value = useContext(PlatformApiContext);
  if (value === null) {
    throw new Error("usePlatformApi needs a PlatformApiProvider");
  }
  return useSessionBound(value);
}
