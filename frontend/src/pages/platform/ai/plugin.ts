import type { ComponentType } from "react";
import type { RouteObject } from "react-router";

import { under } from "../paths.ts";
import type { PlatformSection } from "../sections.ts";
import { MODEL_ROLES_PATH, PROMPTS_PATH, USAGE_PATH } from "./paths.ts";

/**
 * Where the platform AI pages plug into the platform layout (Laysi/trustiot-v2#154: model roles,
 * system prompts and their versions, AI usage). This file is the only contact point:
 *
 * - `aiSections`: one entry for each page, in the "AI" group (`group: "ai"`), with the page's
 *   path under `/platform/ai/...`. The layout draws the ones whose API the build has
 *   (`../available.tsx`) in its section navigation and the narrow screen's select, and marks the
 *   current one.
 * - `aiRoutes`: the routes of the pages whose API is in `api/openapi.json` (the model roles),
 *   relative to `/platform` (`{ path: "ai/model-roles", ... }`). They become children of the
 *   layout route, so they get the guard for platform administrators, the cross-tenant notice and
 *   the navigation without drawing any of it. The pages whose API does not exist yet are in
 *   `preview.ts` (`previewAiPages`), which the production build never imports (`check:dist`
 *   rejects dev-only modules).
 */

/** A page loaded when its route is first matched, as a chunk of its own (as `app/routes.tsx` does). */
function lazyPage<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

export const aiRoutes: RouteObject[] = [
  { path: "ai/model-roles", ...lazyPage(() => import("./ModelRolesPage.tsx"), "ModelRolesPage") },
];

/** The ids of the AI sections, for the providers of their APIs to mount (`PlatformSectionsAvailable`). */
export const MODEL_ROLES_SECTION_ID = "ai-model-roles";
export const PROMPTS_SECTION_ID = "ai-prompts";
export const USAGE_SECTION_ID = "ai-usage";

export const aiSections: readonly PlatformSection[] = [
  {
    id: MODEL_ROLES_SECTION_ID,
    group: "ai",
    label: "platform_ai.section.model_roles",
    path: MODEL_ROLES_PATH,
    covers: (pathname) => under(MODEL_ROLES_PATH, pathname),
  },
  {
    id: PROMPTS_SECTION_ID,
    group: "ai",
    label: "platform_ai.section.prompts",
    path: PROMPTS_PATH,
    covers: (pathname) => under(PROMPTS_PATH, pathname),
  },
  {
    id: USAGE_SECTION_ID,
    group: "ai",
    label: "platform_ai.section.usage",
    path: USAGE_PATH,
    covers: (pathname) => under(USAGE_PATH, pathname),
  },
];
