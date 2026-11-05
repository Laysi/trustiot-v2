import { Outlet } from "react-router";

import { MembersApiProvider } from "../api/pending/MembersApiProvider.tsx";
import { ROUTES, TENANT_PAGES_ROUTE_ID } from "../app/routes.tsx";
import { previewRoutes } from "../pages/members/preview.ts";
import { withRoutes } from "../ui-preview/collect.ts";
import { FakeMembersServer } from "./fake-members.ts";
import { renderAt, signedInBrowser } from "./render.tsx";

/**
 * Renders the member pages (they are preview-only routes, pages/members/preview.ts) at `path`
 * for a signed-in user of `role`, on a members server whose admin is that user. `tenantRoutes`
 * keeps `TENANT_PAGES_ROUTE_ID` referenced, so a rename of the route fails here.
 */
export function renderMembers(
  path: string,
  options: { role?: "viewer" | "editor" | "admin"; setup?(server: FakeMembersServer): void } = {},
) {
  const signed = signedInBrowser(options.role ? { role: options.role } : {});
  const server = new FakeMembersServer({ id: signed.user.id, name: "Alice Admin", email: signed.user.email });
  server.joinedTenantId = signed.tenants[0]?.id ?? server.joinedTenantId;
  options.setup?.(server);
  const routes = withRoutes(ROUTES, [
    {
      id: `members-${TENANT_PAGES_ROUTE_ID}`,
      element: (
        <MembersApiProvider api={server.api()}>
          <Outlet />
        </MembersApiProvider>
      ),
      children: previewRoutes,
    },
  ]);
  return { ...renderAt(path, { routes, identity: signed.browser }), server, signed };
}
