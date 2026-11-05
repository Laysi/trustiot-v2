import "../styles/global.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { startApp } from "../app/start.tsx";
import { PreviewControls } from "./controls.ts";
import { buildPreview, isApiRequest } from "./preview.tsx";
import { loadSettings, type PreviewSettings, saveSettings } from "./settings.ts";
import { Toolbar } from "./Toolbar.tsx";

/**
 * The UI preview's entry (ui-preview.html, `bun run --cwd frontend ui-preview`; frontend/README.md,
 * 「UI 預覽」): the product's app with every API answered by the fakes in the page. Development
 * only: the build's input is index.html, and check:dist refuses any of this in the output.
 *
 * The fakes and the preview-only pages are found here, by file name; nothing else lists them.
 */
const modules = {
  fakes: import.meta.glob("../test/fake-*.ts", { eager: true }),
  pages: import.meta.glob("../pages/**/preview.{ts,tsx}", { eager: true }),
};

function sessionStorageOrUndefined(): Storage | undefined {
  try {
    return window.sessionStorage;
  } catch {
    return undefined;
  }
}

const storage = sessionStorageOrUndefined();
const settings = loadSettings(storage);
const controls = new PreviewControls({ slowMs: settings.slowMs });
const preview = buildPreview(modules, settings, controls);

// The SSE streams call fetch directly, not through the API client: this origin's /api/ and /auth/
// go to the fakes as well, so no request of the app leaves the page.
const networkFetch = window.fetch.bind(window);
const pageFetch = (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
  const request = new Request(input, init);
  return isApiRequest(new URL(request.url), window.location.origin) ? preview.fetch(request) : networkFetch(request);
};
window.fetch = pageFetch as typeof window.fetch;

const toolbar = document.getElementById("ui-preview-toolbar");
if (toolbar === null) throw new Error("ui-preview.html has no #ui-preview-toolbar element");
createRoot(toolbar).render(
  <StrictMode>
    <Toolbar
      settings={settings}
      controls={controls}
      fakes={preview.fakes.map((fake) => fake.declaration.name)}
      onSettings={(next: PreviewSettings) => {
        saveSettings(storage, next);
        // The slow delay applies at once; the session and the seed need fakes created again.
        if (
          next.role !== settings.role ||
          next.platformAdmin !== settings.platformAdmin ||
          next.seed !== settings.seed
        ) {
          window.location.reload();
        }
      }}
    />
  </StrictMode>,
);

startApp({ routes: preview.routes, fetch: preview.fetch, wrap: preview.wrap });
