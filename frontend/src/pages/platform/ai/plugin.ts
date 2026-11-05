import type { ComponentType } from "react";
import type { RouteObject } from "react-router";

import type { MessageId } from "../../../i18n/messages.ts";
import { MODEL_ROLES_PATH, PROMPTS_PATH, USAGE_PATH } from "./paths.ts";

/**
 * Where the platform AI pages plug into the platform administration (Laysi/trustiot-v2#154): the
 * one file the layout of Laysi/trustiot-v2#132 reads (`aiSections` and `aiRoutes`, the shapes its
 * `ai/plugin.ts` asks for). Until that layout is on main, `AiShell` (a thin frame of the same
 * design) wraps the pages that are in the product (`productionAiPages`, in `app/routes.tsx`) and
 * the UI preview wraps the others (`previewAiPages`, in `preview.ts`, with their API pending).
 */

/** A page loaded when its route is first matched, as a chunk of its own (as `app/routes.tsx` does). */
function lazyPage<Name extends string>(load: () => Promise<Record<Name, ComponentType>>, name: Name): RouteObject {
  return { lazy: async () => ({ Component: (await load())[name] }) };
}

/** The pages whose API is in `api/openapi.json`: the model roles. Relative to `/platform/ai`. */
export const productionAiPages: RouteObject[] = [
  { path: "model-roles", ...lazyPage(() => import("./ModelRolesPage.tsx"), "ModelRolesPage") },
];

/**
 * Every AI page of the product, relative to `/platform`, for the platform layout to mount under
 * its route. The pages whose API does not exist yet are not here: they are in `preview.ts`, which
 * the production build never imports (`check:dist` rejects dev-only modules).
 */
export const aiRoutes: RouteObject[] = [{ path: "ai", children: productionAiPages }];

/** An entry of the platform section navigation, in the shape of the layout's `PlatformSection`. */
export interface AiSection {
  id: string;
  group: "ai";
  label: MessageId;
  path: string;
  /** Whether `pathname` is this section's page or below it. */
  covers(pathname: string): boolean;
}

function under(path: string, pathname: string): boolean {
  const normal = pathname.toLowerCase().replace(/\/+$/, "");
  return normal === path || normal.startsWith(`${path}/`);
}

export const aiSections: readonly AiSection[] = [
  {
    id: "ai-model-roles",
    group: "ai",
    label: "platform_ai.section.model_roles",
    path: MODEL_ROLES_PATH,
    covers: (pathname) => under(MODEL_ROLES_PATH, pathname),
  },
  {
    id: "ai-prompts",
    group: "ai",
    label: "platform_ai.section.prompts",
    path: PROMPTS_PATH,
    covers: (pathname) => under(PROMPTS_PATH, pathname),
  },
  {
    id: "ai-usage",
    group: "ai",
    label: "platform_ai.section.usage",
    path: USAGE_PATH,
    covers: (pathname) => under(USAGE_PATH, pathname),
  },
];

/** The sections whose pages are in the product; the UI preview adds the others (`preview.ts`). */
const PRODUCTION_SECTIONS: ReadonlySet<string> = new Set(["ai-model-roles"]);
let pendingPagesOn = false;

/** The UI preview calls this when it mounts the pages that only run on fakes. */
export function enablePendingAiPages(on = true): void {
  pendingPagesOn = on;
}

/** The sections `AiShell` links to: the product's, and in the UI preview the pending ones too. */
export function availableAiSections(): AiSection[] {
  return aiSections.filter((section) => pendingPagesOn || PRODUCTION_SECTIONS.has(section.id));
}
