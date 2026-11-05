import { useCallback, useContext } from "react";
import { matchRoutes, UNSAFE_DataRouterContext } from "react-router";

/**
 * Whether the router has a page for a path, not counting the not-found catch-all. The setup
 * checklist links a step only to a page that exists in this build: the pages of the gateways,
 * the devices to add and the members are built by their own issues, and a link to one that is not
 * there yet would land on the not-found page. The query string and the fragment are ignored.
 *
 * A dynamic or splat route does not count: `/devices/new` also matches `/devices/:deviceId`, and
 * that page would open a device called "new". Only a route spelled out for the path is a page.
 */
export function useRouteExists(): (path: string) => boolean {
  const context = useContext(UNSAFE_DataRouterContext);
  return useCallback(
    (path) => {
      if (context === null) return false;
      const pathname = path.split(/[?#]/)[0] ?? path;
      const matches = matchRoutes(context.router.routes, pathname);
      return matches !== null && matches.length > 0 && matches.every((match) => !/[:*]/.test(match.route.path ?? ""));
    },
    [context],
  );
}
