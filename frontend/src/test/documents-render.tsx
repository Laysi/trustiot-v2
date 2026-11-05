import { Outlet, type RouteObject } from "react-router";

import { DocumentApiProvider } from "../api/pending/DocumentApiProvider.tsx";
import type { DocumentApi } from "../api/pending/documents.ts";
import { ROUTES } from "../app/routes.tsx";
import { previewRoutes } from "../pages/documents/preview.ts";
import { withRoutes } from "../ui-preview/collect.ts";
import { FakeDeviceServer } from "./fake-devices.ts";
import { DEMO_MODELS, FakeDocumentServer, seedDocuments } from "./fake-documents.ts";
import { renderAt, signedInBrowser } from "./render.tsx";

/**
 * Renders the app at `path` with the document library's preview routes added where the preview
 * puts them, and `documents` as the knowledge service: the pages are not in the product's routes
 * (their API is pending), so a test reaches them the way the UI preview does.
 */
export function renderDocuments(path: string, documents: DocumentApi, options: Parameters<typeof renderAt>[1] = {}) {
  const routes: RouteObject[] = [
    {
      element: (
        <DocumentApiProvider api={documents}>
          <Outlet />
        </DocumentApiProvider>
      ),
      children: withRoutes(ROUTES, previewRoutes),
    },
  ];
  return renderAt(path, { ...options, routes });
}

/** The library of the design's demo (`seedDocuments`) over a device server that lists the same three models. */
export function documentLibrary(options: { seeded?: boolean } = {}) {
  const devices = new FakeDeviceServer();
  const models = DEMO_MODELS.map((name) => {
    const model = devices.addModel(name);
    return { id: model.model_id, name };
  });
  const documents = new FakeDocumentServer(models);
  if (options.seeded ?? true) seedDocuments(documents);
  return { devices, documents, models };
}

/** `renderDocuments` for `library`, signed in with `role` (an administrator unless said otherwise). */
export function openDocuments(
  path: string,
  library: ReturnType<typeof documentLibrary>,
  options: { role?: "viewer" | "editor" | "admin" } = {},
) {
  const signedIn = signedInBrowser({ role: options.role ?? "admin" });
  // What the session's user uploads from now on is theirs.
  library.documents.me = signedIn.user.id;
  return renderDocuments(path, library.documents, {
    devices: library.devices.api(),
    identity: signedIn.browser,
  });
}
