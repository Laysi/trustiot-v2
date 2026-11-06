import { BookOpenIcon } from "lucide-react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../app/menu.ts";
import { DOCUMENTS_PATH } from "./paths.ts";

// The document library works on the knowledge service's API, which does not exist yet
// (Laysi/trustiot-v2#193, src/api/pending/documents.ts), so its pages are reachable only in the UI
// preview, never through `routes.tsx` or `menu.ts` (frontend/README.md, 「UI 預覽」). When the API
// lands, both move there.

/** The library and a document's page view, as chunks of their own like the product's pages. */
export const previewRoutes: RouteObject[] = [
  {
    path: DOCUMENTS_PATH.slice(1),
    lazy: async () => ({ Component: (await import("./DocumentsPage.tsx")).DocumentsPage }),
  },
  {
    path: `${DOCUMENTS_PATH.slice(1)}/:documentId`,
    lazy: async () => ({ Component: (await import("./DocumentViewerPage.tsx")).DocumentViewerPage }),
  },
];

/** Group 3 of docs/design/app-shell.md; a page of a document belongs to the library. */
export const previewMenu: MenuItem[] = [
  {
    id: "documents",
    group: 3,
    label: "shell.menu.documents",
    icon: BookOpenIcon,
    path: DOCUMENTS_PATH,
    covers: (pathname) => pathname === DOCUMENTS_PATH || pathname.startsWith(`${DOCUMENTS_PATH}/`),
    // Every member of a tenant reads the library; the plan's refusal is shown on the page.
    visibleTo: (session) => session.tenants.length > 0,
  },
];
