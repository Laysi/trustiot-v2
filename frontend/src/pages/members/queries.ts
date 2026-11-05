// The member page's query keys. Each starts with the active tenant's ID, so a cached answer is
// never shown for another tenant (a tenant switch resets every query that is not the user's).

export const memberListKey = (tenant: string) => ["members", tenant, "list"] as const;

/** The admins' names, for the page a non-admin sees. */
export const adminNamesKey = (tenant: string) => ["members", tenant, "admins"] as const;
