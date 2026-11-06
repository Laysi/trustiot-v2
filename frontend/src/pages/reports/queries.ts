// The reports pages' query keys. Each starts with the active tenant's ID, so a cached answer is
// never shown for another tenant (a tenant switch resets the queries anyway, session/SessionProvider.tsx).

export const schedulesKey = (tenant: string) => ["reports", tenant, "schedules"] as const;

export const scheduleKey = (tenant: string, scheduleId: string) => [...schedulesKey(tenant), scheduleId] as const;

/** Every list of reports, whatever its filter. */
export const reportListsKey = (tenant: string) => ["reports", tenant, "list"] as const;

export const reportListKey = (tenant: string, kind: string) => [...reportListsKey(tenant), kind] as const;

export const reportKey = (tenant: string, reportId: string) => ["reports", tenant, "report", reportId] as const;
