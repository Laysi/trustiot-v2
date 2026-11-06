// Pending Laysi/trustiot-v2#193: replace with a module over the generated client when api/openapi.json has these endpoints.
import { createContext, type ReactNode, useContext } from "react";

import { useSessionBound } from "../../session/PinSession.tsx";
import type { DocumentApi } from "./documents.ts";

const DocumentApiContext = createContext<DocumentApi | null>(null);

/** Gives the document library pages the knowledge service's endpoints: a fake until the service exists. */
export function DocumentApiProvider({ api, children }: { api: DocumentApi; children: ReactNode }) {
  return <DocumentApiContext.Provider value={api}>{children}</DocumentApiContext.Provider>;
}

/** The document API, bound to the page: its requests name the page's user and tenant (`PinSession`). */
export function useDocumentApi(): DocumentApi {
  const value = useContext(DocumentApiContext);
  if (value === null) {
    throw new Error("useDocumentApi needs a DocumentApiProvider");
  }
  return useSessionBound(value);
}
