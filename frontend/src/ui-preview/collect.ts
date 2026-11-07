import type { RouteObject } from "react-router";

import type { MenuItem } from "../app/menu.ts";
import { TENANT_PAGES_ROUTE_ID } from "../app/routes.tsx";
import type { HttpPreviewDeclaration, PreviewDeclaration, ProviderPreviewDeclaration } from "./contract.ts";

/**
 * Turns the modules the preview loads with `import.meta.glob` (file path to module) into fakes,
 * routes and menu entries, checking each declaration, so a mistake in one names its file instead
 * of breaking the preview somewhere later.
 */

export interface LoadedFake {
  file: string;
  declaration: PreviewDeclaration;
}

function fail(file: string, problem: string): never {
  throw new Error(`UI preview: ${file}: ${problem} (frontend/README.md, 「UI 預覽」)`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function sortedEntries(modules: Record<string, unknown>): [string, unknown][] {
  return Object.entries(modules).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
}

function checkDeclaration(file: string, preview: unknown): PreviewDeclaration {
  if (!isRecord(preview)) fail(file, "`preview` is not an object");
  if (typeof preview.name !== "string" || preview.name.trim() === "") fail(file, "`preview.name` is not a name");
  const hasHttp = "http" in preview && preview.http !== undefined;
  const hasProvider = "provider" in preview && preview.provider !== undefined;
  if (hasHttp === hasProvider) fail(file, "`preview` needs exactly one of `http` and `provider`");
  if (hasProvider) {
    if (typeof preview.provider !== "function") fail(file, "`preview.provider` is not a function");
    return preview as unknown as ProviderPreviewDeclaration;
  }
  const http = preview.http;
  if (!isRecord(http) || typeof http.create !== "function") fail(file, "`preview.http.create` is not a function");
  const prefixes = http.prefixes;
  if (
    !Array.isArray(prefixes) ||
    prefixes.length === 0 ||
    !prefixes.every((prefix) => typeof prefix === "string" && prefix.startsWith("/"))
  ) {
    fail(file, "`preview.http.prefixes` must be paths that start with /");
  }
  const mixed = prefixes.find((prefix: string) => prefix.split("/").some((part) => part !== "*" && part.includes("*")));
  if (mixed !== undefined) fail(file, `the prefix ${mixed} has a * that is not a whole path segment`);
  return preview as unknown as HttpPreviewDeclaration;
}

/** The fakes that declare `preview`, in file order; a module without it (fake-chart.ts) is skipped. */
export function collectFakes(modules: Record<string, unknown>): LoadedFake[] {
  const fakes: LoadedFake[] = [];
  const owners = new Map<string, string>();
  for (const [file, module] of sortedEntries(modules)) {
    if (!isRecord(module) || !("preview" in module)) continue;
    const declaration = checkDeclaration(file, module.preview);
    if ("http" in declaration) {
      for (const prefix of declaration.http.prefixes) {
        const owner = owners.get(prefix);
        if (owner !== undefined) fail(file, `the prefix ${prefix} is already answered by ${owner}`);
        owners.set(prefix, file);
      }
    }
    fakes.push({ file, declaration });
  }
  return fakes;
}

function checkMenuItem(file: string, item: unknown): MenuItem {
  if (
    !isRecord(item) ||
    typeof item.id !== "string" ||
    typeof item.group !== "number" ||
    typeof item.label !== "string" ||
    typeof item.path !== "string" ||
    typeof item.covers !== "function" ||
    typeof item.visibleTo !== "function" ||
    item.icon === undefined
  ) {
    fail(file, "an entry of `previewMenu` is not a MenuItem (app/menu.ts)");
  }
  return item as unknown as MenuItem;
}

/** The preview-only routes and menu entries of every page directory's preview.ts, in file order. */
export function collectPages(
  modules: Record<string, unknown>,
  base: readonly MenuItem[],
): { routes: RouteObject[]; menu: MenuItem[] } {
  const routes: RouteObject[] = [];
  const menu: MenuItem[] = [];
  const ids = new Map(base.map((item) => [item.id, "app/menu.ts"]));
  for (const [file, module] of sortedEntries(modules)) {
    if (!isRecord(module)) fail(file, "is not a module");
    const { previewRoutes = [], previewMenu = [] } = module;
    if (!Array.isArray(previewRoutes) || !previewRoutes.every(isRecord)) fail(file, "`previewRoutes` is not a list");
    if (!Array.isArray(previewMenu)) fail(file, "`previewMenu` is not a list");
    routes.push(...(previewRoutes as RouteObject[]));
    for (const entry of previewMenu) {
      const item = checkMenuItem(file, entry);
      const owner = ids.get(item.id);
      if (owner !== undefined) fail(file, `the menu entry ${item.id} is already in ${owner}`);
      ids.set(item.id, file);
      menu.push(item);
    }
  }
  return { routes, menu };
}

/**
 * `routes` with `extra` added under the route of the tenant's pages (`TENANT_PAGES_ROUTE_ID`):
 * the preview-only pages get the same shell, session and tenant gate as the product's. Only the
 * routes on the way there are copied; `routes` itself is left as it is.
 */
export function withRoutes(routes: readonly RouteObject[], extra: readonly RouteObject[]): RouteObject[] {
  let found = false;
  const visit = (list: readonly RouteObject[]): RouteObject[] =>
    list.map((route) => {
      if (route.id === TENANT_PAGES_ROUTE_ID) {
        found = true;
        return { ...route, children: [...(route.children ?? []), ...extra] } as RouteObject;
      }
      return route.children ? ({ ...route, children: visit(route.children) } as RouteObject) : route;
    });
  const result = visit(routes);
  if (!found) throw new Error(`UI preview: no route has the id ${TENANT_PAGES_ROUTE_ID} (app/routes.tsx)`);
  return result;
}
