import type { ComponentType, ReactNode } from "react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../app/menu.ts";
import type { MemberRole } from "../session/session.ts";

/**
 * What the UI preview reads from the files it loads by itself (frontend/README.md, 「UI 預覽」):
 *
 * - every `src/test/fake-*.ts` that exports `preview` (a `PreviewDeclaration`): a fake of an API
 *   that exists answers its paths at the HTTP level (`http`), a fake of an API whose backend does
 *   not exist yet is a provider around the app (`provider`);
 * - every `src/pages/<area>/preview.ts`, which exports `previewRoutes` and `previewMenu`: the routes
 *   and menu entries of pages that are only in the preview.
 *
 * A fake module needs no import from here: an object literal of this shape is enough.
 */

/** Which data the fakes start with: what the design draws as the normal state, or nothing. */
export type PreviewSeed = "normal" | "empty";
export const PREVIEW_SEEDS: readonly PreviewSeed[] = ["normal", "empty"];

/**
 * Who the preview's session is. The identity fake's session carries it, and pages read it from the
 * session as in the product. A fake that answers by the caller's role, as its server would (the
 * dashboards' `permissions`), may read it from here: it is the session's.
 */
export interface PreviewContext {
  role: MemberRole;
  platformAdmin: boolean;
}

export type PreviewFetch = (request: Request) => Promise<Response>;

export interface HttpPreviewDeclaration {
  /** Shown in the preview's toolbar. */
  name: string;
  http: {
    // The paths it answers: a request goes to the fake with the most specific prefix of its path
    // (the most segments). A segment that is just an asterisk matches exactly one path segment, so
    // a fake can answer the dashboards below one conversation (`.../conversations/<*>/dashboards`,
    // written with the asterisk) while another fake owns `.../conversations` itself.
    prefixes: readonly string[];
    create: (seed: PreviewSeed, context: PreviewContext) => { fetch: PreviewFetch };
  };
}

export interface ProviderPreviewDeclaration {
  name: string;
  provider: (seed: PreviewSeed, context: PreviewContext) => ComponentType<{ children: ReactNode }>;
}

export type PreviewDeclaration = HttpPreviewDeclaration | ProviderPreviewDeclaration;

/** What a page directory's preview.ts exports. */
export interface PreviewPagesModule {
  previewRoutes?: RouteObject[];
  previewMenu?: MenuItem[];
}
