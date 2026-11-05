import { UsersIcon } from "lucide-react";
import type { RouteObject } from "react-router";

import type { MenuItem } from "../../app/menu.ts";
import { INVITATION_ACCEPT_PATH, MEMBERS_PATH } from "./paths.ts";

/**
 * The member pages exist only in the UI preview (frontend/README.md, 「UI 預覽」): the identity
 * service has no member or invitation endpoints yet (Laysi/trustiot-v2#105), so they run on the
 * fake of `src/api/pending/members.ts`. When the endpoints land, the pages move to the real API
 * and their route and menu entry move to `app/routes.tsx` and `app/menu.ts`.
 */
export const previewRoutes: RouteObject[] = [
  {
    path: MEMBERS_PATH.slice(1),
    lazy: async () => ({ Component: (await import("./MembersPage.tsx")).MembersPage }),
  },
  {
    // In the product this route goes beside the tenant gate, not under it: a person invited to
    // their first organization has none yet.
    path: INVITATION_ACCEPT_PATH.slice(1),
    lazy: async () => ({ Component: (await import("./AcceptInvitationPage.tsx")).AcceptInvitationPage }),
  },
];

export const previewMenu: MenuItem[] = [
  {
    id: "members",
    group: 4,
    label: "shell.menu.members",
    icon: UsersIcon,
    path: MEMBERS_PATH,
    covers: (pathname) => pathname === MEMBERS_PATH || pathname.startsWith(`${MEMBERS_PATH}/`),
    // Members are an admin's page (design decision 1); everyone else does not see the entry.
    visibleTo: (session) => session.activeTenant?.role === "admin",
  },
];
