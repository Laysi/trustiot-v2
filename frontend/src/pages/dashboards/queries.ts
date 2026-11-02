// The dashboard pages' query keys. Each starts with the active tenant's ID, so a cached answer is
// never shown for another tenant; a tenant switch resets every query that is not the user's
// anyway (session/SessionProvider.tsx). A dashboard's versions and dataset runs sit under its key,
// so refreshing the dashboard after a restore or an archive refreshes them too.

export const dashboardListKey = (tenant: string, includeArchived: boolean) =>
  ["dashboards", tenant, "list", includeArchived] as const;

export const dashboardKey = (tenant: string, dashboardId: string) => ["dashboards", tenant, dashboardId] as const;

/** A version of the dashboard; `latest` without `?version=`. */
export const dashboardVersionKey = (tenant: string, dashboardId: string, version: number | "latest") =>
  [...dashboardKey(tenant, dashboardId), "version", version] as const;

export const versionListKey = (tenant: string, dashboardId: string) =>
  [...dashboardKey(tenant, dashboardId), "versions"] as const;

/** Every dataset run of one version; a single run adds the dataset's name. */
export const runsKey = (tenant: string, dashboardId: string, version: number) =>
  [...dashboardKey(tenant, dashboardId), "runs", version] as const;
