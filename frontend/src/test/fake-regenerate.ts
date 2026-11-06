import { createElement } from "react";

import { createApiClient } from "../api/client.ts";
import { regenerateApi } from "../api/pending/regenerate.ts";
import { RegenerateAvailable } from "../pages/conversations/regenerate.tsx";
import type { ProviderPreviewDeclaration } from "../ui-preview/contract.ts";
import type { FakeQuestionServer } from "./fake-questions.ts";

/**
 * The client side of the regenerate endpoint of Laysi/trustiot-v2#649 (api/pending/regenerate.ts),
 * which the question server (test/fake-questions.ts, `regenerate`) answers at the HTTP level.
 * `regenerateFor` is the endpoint over a server, as a test (or another tab) calls it.
 */
export function regenerateFor(server: FakeQuestionServer) {
  return regenerateApi(createApiClient({ fetch: (request) => server.fetch(request) }));
}

/**
 * The UI preview's regenerate button (a provider: the endpoint is pending, so the conversation page
 * shows the button only here). The requests go through the page's `fetch`, which the preview
 * routes to the conversations fake, so the fake discards and asks again as the service does. In
 * the preview, a question with `!fail` or `!slow` in it does the same when it is regenerated.
 */
export const preview: ProviderPreviewDeclaration = {
  name: "regenerate",
  provider: () => {
    const api = regenerateApi(createApiClient({ fetch: (request) => window.fetch(request) }));
    return ({ children }) => createElement(RegenerateAvailable, { api, children });
  },
};
