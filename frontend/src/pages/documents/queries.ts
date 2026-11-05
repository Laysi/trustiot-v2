// The document library's query keys. Each starts with the active tenant's ID, so a cached answer
// is never shown for another tenant.

export const documentListKey = (tenant: string) => ["documents", tenant, "list"] as const;

export const documentKey = (tenant: string, documentId: string) => ["documents", tenant, documentId] as const;

export const pageKey = (tenant: string, documentId: string, version: number, page: number) =>
  [...documentKey(tenant, documentId), "page", version, page] as const;

/** How often the list and a document are read again while a version is waiting or being processed. */
export const PROCESSING_POLL_MS = 3000;
