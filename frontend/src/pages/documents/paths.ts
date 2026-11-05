/**
 * The document library; one document's page view is `/documents/<document ID>`, which the
 * citations of the assistant's answers link to with `?version=<n>&page=<n>&box=x,y,w,h`
 * (docs/design/ms4-ms5/design.md, 「深連結」).
 */
export const DOCUMENTS_PATH = "/documents";

/** The page view of a document: `version` and `page` pick the page image, `box` the cited area. */
export function documentPath(
  documentId: string,
  where: { version?: number; page?: number; box?: { x: number; y: number; width: number; height: number } } = {},
): string {
  const path = `${DOCUMENTS_PATH}/${encodeURIComponent(documentId)}`;
  const params = new URLSearchParams();
  if (where.version !== undefined) params.set("version", String(where.version));
  if (where.page !== undefined) params.set("page", String(where.page));
  if (where.box !== undefined) {
    params.set("box", [where.box.x, where.box.y, where.box.width, where.box.height].join(","));
  }
  const query = params.toString();
  return query === "" ? path : `${path}?${query}`;
}
