import type { ComponentType, ReactElement, ReactNode } from "react";
import type { RouteObject } from "react-router";

import { MENU_ITEMS, type MenuItem, MenuItemsContext } from "../app/menu.ts";
import { ROUTES } from "../app/routes.tsx";
import { collectFakes, collectPages, type LoadedFake, withRoutes } from "./collect.ts";
import type { PreviewContext, PreviewFetch } from "./contract.ts";
import { type PreviewControls, type PreviewRoute, routeRequests } from "./controls.ts";
import type { PreviewSettings } from "./settings.ts";

/** The modules `import.meta.glob` loaded: file path to module. */
export interface PreviewModules {
  fakes: Record<string, unknown>;
  pages: Record<string, unknown>;
}

/** What the preview gives `startApp`, and the menu and fakes it built them from. */
export interface Preview {
  /** The product's routes plus every preview.ts's routes, under the tenant's pages. */
  routes: RouteObject[];
  /** The product's menu plus every preview.ts's entries. */
  menu: readonly MenuItem[];
  /** The API client's fetch: the HTTP fakes, by the longest prefix. */
  fetch: PreviewFetch;
  /** The app inside the menu and every provider fake. */
  wrap: (app: ReactElement) => ReactElement;
  fakes: LoadedFake[];
}

/**
 * Builds the preview from the loaded modules: one instance of every fake, created with the
 * settings' seed and session, and the routes and menu of the product plus the preview-only pages.
 * Only the routes, the menu, the API client's fetch and the providers around the app differ from
 * main.tsx; the router, the shell and the pages are the product's.
 */
export function buildPreview(modules: PreviewModules, settings: PreviewSettings, controls: PreviewControls): Preview {
  const context: PreviewContext = { role: settings.role, platformAdmin: settings.platformAdmin };
  const fakes = collectFakes(modules.fakes);
  const httpRoutes: PreviewRoute[] = [];
  const providers: { file: string; Provider: ComponentType<{ children: ReactNode }> }[] = [];
  for (const { file, declaration } of fakes) {
    if ("http" in declaration) {
      const instance = declaration.http.create(settings.seed, context);
      const fetch: PreviewFetch = (request) => instance.fetch(request);
      for (const prefix of declaration.http.prefixes) httpRoutes.push({ prefix, fetch });
    } else {
      providers.push({ file, Provider: declaration.provider(settings.seed, context) });
    }
  }
  const pages = collectPages(modules.pages, MENU_ITEMS);
  const menu: readonly MenuItem[] = [...MENU_ITEMS, ...pages.menu];
  const wrap = (app: ReactElement) =>
    providers.reduceRight<ReactElement>(
      (inner, { file, Provider }) => <Provider key={file}>{inner}</Provider>,
      <MenuItemsContext.Provider value={menu}>{app}</MenuItemsContext.Provider>,
    );
  return {
    routes: withRoutes(ROUTES, pages.routes),
    menu,
    fetch: routeRequests(httpRoutes, controls),
    wrap,
    fakes,
  };
}

/** Whether `url` is a request the fakes answer: this origin's /api/ and /auth/ (the SSE streams too). */
export function isApiRequest(url: URL, origin: string): boolean {
  return url.origin === origin && (url.pathname.startsWith("/api/") || url.pathname.startsWith("/auth/"));
}
