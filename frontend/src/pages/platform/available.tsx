import { createContext, type ReactNode, useContext, useMemo } from "react";

const AvailableSectionsContext = createContext<ReadonlySet<string>>(new Set());

/**
 * Says to the platform layout below that the sections `ids` (`sections.ts`) have their API in this
 * build. Whoever provides a section's API mounts it: `ModelRoleApiProvider` in the app, and the
 * providers of the pending APIs (`src/api/pending/`) in the UI preview and in tests. So the
 * product's navigation lists only the pages the product has, and a pending page's section appears
 * in the same change that moves its provider into `AppProviders`. It lives here, not next to the
 * pending APIs, because the product's layout reads it: the build must not carry
 * `src/api/pending/` (`check:dist`). Providers nest: each adds its ids to those above it.
 */
export function PlatformSectionsAvailable({ ids, children }: { ids: readonly string[]; children: ReactNode }) {
  const above = useContext(AvailableSectionsContext);
  const value = useMemo(() => new Set([...above, ...ids]), [above, ids]);
  return <AvailableSectionsContext.Provider value={value}>{children}</AvailableSectionsContext.Provider>;
}

/** The ids of the platform sections whose API this build has. */
export function useAvailablePlatformSections(): ReadonlySet<string> {
  return useContext(AvailableSectionsContext);
}
