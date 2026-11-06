import { Outlet, type RouteObject } from "react-router";

import { PlatformApiProvider } from "../api/pending/PlatformApiProvider.tsx";
import type { PlatformApi } from "../api/pending/platform.ts";
import { Layout } from "../app/Layout.tsx";
import { PageLoading } from "../app/PageLoading.tsx";
import { ErrorPage } from "../pages/ErrorPage.tsx";
import { NotFoundPage } from "../pages/pages.tsx";
import { platformRoutes } from "../pages/platform/routes.ts";
import type { FakePlatform } from "./fake-platform.ts";
import { renderAt, signedInBrowser } from "./render.tsx";

/**
 * Renders the platform back office at `path` as the preview mounts it: the app's frame, the fake
 * platform API around the pages, and the pages' own routes (the layout with its guard). The user is
 * a platform administrator unless `platformAdmin` says otherwise. The routes sit outside
 * `TenantGate`, as they will in the product.
 */
export function renderPlatform(
  path: string,
  options: {
    api: FakePlatform | PlatformApi;
    platformAdmin?: boolean;
    tenants?: readonly (string | null)[];
    /** Changes the identity fake before the page is rendered (a tenant of the session suspended). */
    prepare?: (signedIn: ReturnType<typeof signedInBrowser>) => void;
  },
) {
  const routes: RouteObject[] = [
    {
      path: "/",
      element: <Layout />,
      HydrateFallback: PageLoading,
      errorElement: <ErrorPage />,
      children: [
        {
          element: (
            <PlatformApiProvider api={options.api}>
              <Outlet />
            </PlatformApiProvider>
          ),
          errorElement: <ErrorPage />,
          children: [...platformRoutes, { path: "*", element: <NotFoundPage /> }],
        },
      ],
    },
  ];
  const signedIn = signedInBrowser({
    platformAdmin: options.platformAdmin ?? true,
    ...(options.tenants ? { tenants: options.tenants } : {}),
  });
  options.prepare?.(signedIn);
  return { ...signedIn, ...renderAt(path, { routes, identity: signedIn.browser }) };
}
