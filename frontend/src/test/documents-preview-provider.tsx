import { type ReactNode, useEffect } from "react";

import { DeviceApiProvider } from "../api/DeviceApiProvider.tsx";
import type { DeviceApi } from "../api/devices.ts";
import { DocumentApiProvider } from "../api/pending/DocumentApiProvider.tsx";
import type { FakeDocumentServer } from "./fake-documents.ts";

/**
 * The providers the UI preview wraps the app in for the document library: the knowledge service's
 * fake, and a device API that lists the same models (the page reads model names through the real
 * device API). The clock that moves processing along runs only while this is mounted. `children`
 * is optional so a `.ts` file can build it with `createElement` (fake-documents.ts).
 */
export function DocumentsPreviewProvider({
  documents,
  devices,
  children,
}: {
  documents: FakeDocumentServer;
  devices: DeviceApi;
  children?: ReactNode;
}) {
  useEffect(() => {
    documents.startProcessing();
    return () => documents.stop();
  }, [documents]);
  return (
    <DeviceApiProvider api={devices}>
      <DocumentApiProvider api={documents}>{children}</DocumentApiProvider>
    </DeviceApiProvider>
  );
}
