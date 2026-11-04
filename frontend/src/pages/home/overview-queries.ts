import { useQuery } from "@tanstack/react-query";

import { useHomeOverviewApi } from "../../api/pending/home-overview.ts";
import { usePreferences } from "../../preferences/PreferencesProvider.tsx";
import { useSession } from "../../session/SessionProvider.tsx";

// The generic home's query keys, one per block so that each fails and retries on its own (the
// design's 12-E). Each starts with the tenant's ID, like every page's.
const overviewKey = (tenant: string, part: string, tz: string) => ["overview", tenant, part, tz] as const;

/** Every query of the overview, to refresh them together. */
export const overviewKeys = (tenant: string) => ["overview", tenant] as const;

function useTenantAndZone() {
  const tenant = useSession().session?.activeTenant?.id ?? "";
  const { timeZone } = usePreferences();
  return { tenant, tz: timeZone };
}

export function useStatusSummary() {
  const api = useHomeOverviewApi();
  const { tenant, tz } = useTenantAndZone();
  return useQuery({
    queryKey: overviewKey(tenant, "summary", tz),
    queryFn: ({ signal }) => api.getStatusSummary(tz, signal),
  });
}

export function useTopUsage() {
  const api = useHomeOverviewApi();
  const { tenant, tz } = useTenantAndZone();
  return useQuery({ queryKey: overviewKey(tenant, "usage", tz), queryFn: ({ signal }) => api.getTopUsage(tz, signal) });
}

export function useNewAnomalies() {
  const api = useHomeOverviewApi();
  const { tenant, tz } = useTenantAndZone();
  return useQuery({
    queryKey: overviewKey(tenant, "anomalies", tz),
    queryFn: ({ signal }) => api.getNewAnomalies(signal),
  });
}

export function useDeviceHealth() {
  const api = useHomeOverviewApi();
  const { tenant, tz } = useTenantAndZone();
  return useQuery({
    queryKey: overviewKey(tenant, "devices", tz),
    queryFn: ({ signal }) => api.listDeviceHealth(tz, signal),
  });
}
