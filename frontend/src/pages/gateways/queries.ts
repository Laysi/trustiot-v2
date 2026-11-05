// The gateway page's query keys. Each starts with the active tenant's ID, so a cached answer is
// never shown for another tenant (a tenant switch resets every query that is not the user's).

export const gatewaysKey = (tenant: string) => ["gateways", tenant, "list"] as const;
export const gatewayHealthKey = (tenant: string) => ["gateways", tenant, "health"] as const;
export const gatewayAllowanceKey = (tenant: string) => ["gateways", tenant, "allowance"] as const;
