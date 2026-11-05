import type { RouteObject } from "react-router";

import type { PlatformSection } from "../sections.ts";

/**
 * Where the platform AI pages plug into the platform layout (Laysi/trustiot-v2#154: model roles,
 * system prompts and their versions, AI usage). This file is the only contact point:
 *
 * - `aiSections`: one entry for each page, in the "AI" group (`group: "ai"`), with the page's
 *   path under `/platform/ai/...`. The layout draws them in its section navigation and the narrow
 *   screen's select, and marks the current one.
 * - `aiRoutes`: the pages' routes, relative to `/platform` (`{ path: "ai/model-roles", ... }`).
 *   They become children of the layout route, so they get the guard for platform administrators,
 *   the cross-tenant notice and the navigation without drawing any of it.
 *
 * Nothing else in this folder is needed: the AI pages have their own API, provider and fake.
 */
export const aiSections: readonly PlatformSection[] = [];
export const aiRoutes: RouteObject[] = [];
