import type { ComponentType } from "react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../../app/menu.ts";
import { AiShell } from "./AiShell.tsx";
import { PLATFORM_AI_PATH } from "./paths.ts";
import { enablePendingAiPages } from "./plugin.ts";

/** A page loaded when its route is first matched, as a chunk of its own (as `app/routes.tsx` does). */
function lazyPage<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

/**
 * The pages whose API does not exist yet (system prompts: Laysi/trustiot-v2#153, usage:
 * Laysi/trustiot-v2#140), so they run on `fake-prompts.ts` and `fake-ai-usage.ts` and are only in
 * the UI preview. Relative to `/platform/ai`.
 */
export const previewAiPages: RouteObject[] = [
  { path: "prompts", ...lazyPage(() => import("./PromptsPage.tsx"), "PromptsPage") },
  { path: "prompts/:service/:role", ...lazyPage(() => import("./PromptsPage.tsx"), "PromptsPage") },
  {
    path: "prompts/:service/:role/versions",
    ...lazyPage(() => import("./PromptVersionsPage.tsx"), "PromptVersionsPage"),
  },
  { path: "usage", ...lazyPage(() => import("./AiUsagePage.tsx"), "AiUsagePage") },
];

// The AI sections of the frame list these pages from now on.
enablePendingAiPages();

/**
 * The model roles page is a product route (`app/routes.tsx`), so the preview shows it too; this
 * adds the other pages next to it. The platform administration's menu entry belongs to the
 * platform layout (Laysi/trustiot-v2#132), so there is none here.
 */
export const previewRoutes: RouteObject[] = [
  { path: PLATFORM_AI_PATH.slice(1), Component: AiShell, children: previewAiPages },
];

export const previewMenu: MenuItem[] = [];
