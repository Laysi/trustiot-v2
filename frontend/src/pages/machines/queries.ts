// The machine pages' query keys. Each starts with the active tenant's ID, so a cached answer is
// never shown for another tenant. Every write invalidates `machinesKey(tenant)`: the list, the
// detail, the records and the change record all hang under it.

export const machinesKey = (tenant: string) => ["machines", tenant] as const;

export const machineListKey = (tenant: string) => [...machinesKey(tenant), "list"] as const;

/** A machine's ID is a UUID, never `list` or `assignments`. */
export const machineKey = (tenant: string, machineId: string) => [...machinesKey(tenant), machineId] as const;

export const runsKey = (tenant: string, machineId: string, from: number) =>
  [...machineKey(tenant, machineId), "runs", from] as const;

export const changesKey = (tenant: string, machineId: string) => [...machineKey(tenant, machineId), "changes"] as const;

export const assignmentsKey = (tenant: string) => [...machinesKey(tenant), "assignments"] as const;
