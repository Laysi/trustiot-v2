import type { ComponentType } from "react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../../app/menu.ts";

/** A page loaded when its route is first matched, as a chunk of its own (as `app/routes.tsx` does). */
function lazyPage<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

/**
 * The pages whose API does not exist yet (system prompts: Laysi/trustiot-v2#153, usage:
 * Laysi/trustiot-v2#140), so they run on `fake-prompts.ts` and `fake-ai-usage.ts` and are only in
 * the UI preview. Relative to `/platform`, like `aiRoutes` in `plugin.ts`; the platform back
 * office's preview routes (`../preview.ts`) mount them in its layout.
 */
export const previewAiPages: RouteObject[] = [
  { path: "ai/prompts", ...lazyPage(() => import("./PromptsPage.tsx"), "PromptsPage") },
  { path: "ai/prompts/:service/:role", ...lazyPage(() => import("./PromptsPage.tsx"), "PromptsPage") },
  {
    path: "ai/prompts/:service/:role/versions",
    ...lazyPage(() => import("./PromptVersionsPage.tsx"), "PromptVersionsPage"),
  },
  { path: "ai/usage", ...lazyPage(() => import("./AiUsagePage.tsx"), "AiUsagePage") },
];

/**
 * Nothing of its own: the pages above are mounted by `../preview.ts`, inside the platform layout,
 * and the platform back office's menu entry is there too.
 */
export const previewRoutes: RouteObject[] = [];

export const previewMenu: MenuItem[] = [];
