import { Outlet } from "react-router";

import { AiUsageApiProvider } from "../api/pending/AiUsageApiProvider.tsx";
import type { AiUsageApi } from "../api/pending/aiUsage.ts";
import { PromptApiProvider } from "../api/pending/PromptApiProvider.tsx";
import type { PromptApi } from "../api/pending/prompts.ts";
import { Layout } from "../app/Layout.tsx";
import { aiRoutes } from "../pages/platform/ai/plugin.ts";
import { previewAiPages } from "../pages/platform/ai/preview.ts";
import { platformLayoutRoute } from "../pages/platform/layout-route.ts";
import { FakeAiUsageServer } from "./fake-ai-usage.ts";
import { FakePromptServer } from "./fake-prompts.ts";
import { renderAt, signedInBrowser } from "./render.tsx";

/**
 * The platform AI pages (the model roles of `pages/platform/ai/plugin.ts` and the preview-only
 * pages of `preview.ts`) in the app's layout and the platform layout, with the fakes of their
 * pending APIs around them, signed in as a platform administrator unless the test says otherwise.
 */
export function renderPlatformAi(
  path: string,
  options: { prompts?: PromptApi; usage?: AiUsageApi; platformAdmin?: boolean } = {},
) {
  const prompts = options.prompts ?? new FakePromptServer().api();
  const usage = options.usage ?? new FakeAiUsageServer().api();
  const { browser } = signedInBrowser({ platformAdmin: options.platformAdmin ?? true });
  return renderAt(path, {
    identity: browser,
    routes: [
      {
        path: "/",
        element: <Layout />,
        children: [
          {
            element: (
              <PromptApiProvider api={prompts}>
                <AiUsageApiProvider api={usage}>
                  <Outlet />
                </AiUsageApiProvider>
              </PromptApiProvider>
            ),
            children: [platformLayoutRoute([...aiRoutes, ...previewAiPages])],
          },
        ],
      },
    ],
  });
}
