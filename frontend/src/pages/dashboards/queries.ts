// The dashboard pages' query keys. Each starts with the active tenant's ID, so a cached answer is
// never shown for another tenant; a tenant switch resets every query that is not the user's
// anyway (session/SessionProvider.tsx). A dashboard's versions and dataset runs sit under its key.
// What a write refreshes is a narrower key than the dashboard's own: the views and the version
// list, never the dataset runs, which belong to the viewing and run again only when the user asks.

/** Every list of dashboards, whether archived ones are asked for or not. */
export const dashboardListsKey = (tenant: string) => ["dashboards", tenant, "list"] as const;

export const dashboardListKey = (tenant: string, includeArchived: boolean) =>
  [...dashboardListsKey(tenant), includeArchived] as const;

export const dashboardKey = (tenant: string, dashboardId: string) => ["dashboards", tenant, dashboardId] as const;

/** Every view of the dashboard: its latest version and each version asked for by number. */
export const dashboardViewsKey = (tenant: string, dashboardId: string) =>
  [...dashboardKey(tenant, dashboardId), "version"] as const;

/** A version of the dashboard; `latest` without `?version=`. */
export const dashboardVersionKey = (tenant: string, dashboardId: string, version: number | "latest") =>
  [...dashboardViewsKey(tenant, dashboardId), version] as const;

export const versionListKey = (tenant: string, dashboardId: string) =>
  [...dashboardKey(tenant, dashboardId), "versions"] as const;

/** Every dataset run of one version; a single run adds the dataset's name. */
export const runsKey = (tenant: string, dashboardId: string, version: number) =>
  [...dashboardKey(tenant, dashboardId), "runs", version] as const;
