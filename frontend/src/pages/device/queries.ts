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

export const modelKey = (tenant: string, modelId: string) => ["device-models", tenant, modelId] as const;
