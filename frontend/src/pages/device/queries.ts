// The device page's query keys. Each starts with the active tenant's ID, so a cached answer is
// never shown for another tenant; a tenant switch resets every query that is not the user's
// anyway (session/SessionProvider.tsx). The latest values and the series sit under the device's
// key, so refreshing the device refreshes them too.

export const deviceKey = (tenant: string, deviceId: string) => ["devices", tenant, deviceId] as const;

export const latestKey = (tenant: string, deviceId: string) => [...deviceKey(tenant, deviceId), "latest"] as const;

export const seriesKey = (
  tenant: string,
  deviceId: string,
  query: { point: string; from: number; to: number; tz: string },
) => [...deviceKey(tenant, deviceId), "series", query.point, query.from, query.to, query.tz] as const;

/** Whether a query key is one of a device's series (a range's history), as `seriesKey` builds it. */
export const isSeriesKey = (key: readonly unknown[]) => key[3] === "series";

export const modelKey = (tenant: string, modelId: string) => ["device-models", tenant, modelId] as const;

/** The tenant's device list and model list (the devices page); a device's key is never `list`, an ID is a UUID. */
export const deviceListKey = (tenant: string) => ["devices", tenant, "list"] as const;

export const modelListKey = (tenant: string) => ["device-models", tenant, "list"] as const;

/** Every device query of one tenant (the list, each device, its readings): what a device write refreshes. */
export const tenantDevicesKey = (tenant: string) => ["devices", tenant] as const;

/** Every model query of one tenant (the list and each model): what a model write refreshes. */
export const tenantModelsKey = (tenant: string) => ["device-models", tenant] as const;

/** The tenant's gateways and its plan's polling limit, which the device form chooses from. */
export const gatewayChoicesKey = (tenant: string) => ["gateways", tenant, "choices"] as const;

export const planLimitsKey = (tenant: string) => ["plan-limits", tenant] as const;
