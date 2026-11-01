import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, screen } from "@testing-library/react";
import type { RouteObject } from "react-router";

import { ApiError } from "../api/errors.ts";
import { ErrorPage } from "../pages/ErrorPage.tsx";
import { FakeIdentity } from "../test/fake-identity.ts";
import { memoryPreferences, renderAt, signedInBrowser } from "../test/render.tsx";
import { Layout } from "./Layout.tsx";
import { PageLoading } from "./PageLoading.tsx";
import { isReservedPath } from "./paths.ts";
import { ROUTES } from "./routes.tsx";

afterEach(() => {
  cleanup();
});

/** Every route's full path pattern, parents included. */
function routePaths(routes: readonly RouteObject[], parent = ""): string[] {
  return routes.flatMap((route) => {
    const own =
      route.path === undefined
        ? parent
        : route.path.startsWith("/")
          ? route.path
          : `${parent.replace(/\/$/, "")}/${route.path}`;
    return [...(route.path === undefined ? [] : [own]), ...routePaths(route.children ?? [], own)];
  });
}

describe("routes", () => {
  test("no route lies under a path the ingress or the web service keeps for itself", () => {
    const paths = routePaths(ROUTES);
    expect(paths).toContain("/");
    expect(paths).toContain("/login");
    expect(paths).toContain("/settings");
    expect(paths.filter((path) => path !== "/*" && isReservedPath(path.replace(/\/\*$/, "")))).toEqual([]);
  });

  test("every top-level route has an error boundary", () => {
    expect(ROUTES.every((route) => route.errorElement !== undefined)).toBe(true);
  });

  test("the home page renders inside the layout", async () => {
    renderAt("/", { routes: ROUTES });
    expect(await screen.findByRole("heading", { name: "Home" })).toBeDefined();
    expect(screen.getByRole("navigation", { name: "Main navigation" })).toBeDefined();
  });

  test("a deep link to a page that loads lazily renders it inside the layout", async () => {
    renderAt("/settings", { routes: ROUTES });
    expect(await screen.findByRole("heading", { name: "Settings" })).toBeDefined();
    expect(screen.getByRole("navigation", { name: "Main navigation" })).toBeDefined();
  });

  test("a deep link renders as if the page had been reloaded there", async () => {
    // Any path outside the reserved ones loads index.html (services/web), and the router takes
    // over from the address bar: an unknown page shows "not found" inside the layout.
    renderAt("/devices/42/history?range=7d", { routes: ROUTES });
    expect(await screen.findByRole("heading", { name: "Page not found" })).toBeDefined();
    expect(screen.getByText("The page /devices/42/history doesn't exist or has moved.")).toBeDefined();
    expect(screen.getByRole("navigation", { name: "Main navigation" })).toBeDefined();
  });

  test("the sign-in page renders without the layout", async () => {
    renderAt("/login?next=%2Fdevices", { routes: ROUTES, identity: new FakeIdentity().browser() });
    expect(await screen.findByRole("heading", { name: "Sign in to TrustIoT" })).toBeDefined();
    expect(screen.queryByRole("navigation", { name: "Main navigation" })).toBeNull();
  });

  test("a visitor without a session is sent to sign in, and back to the page afterwards", async () => {
    const { router } = renderAt("/settings?tab=account", { routes: ROUTES, identity: new FakeIdentity().browser() });
    expect(await screen.findByRole("heading", { name: "Sign in to TrustIoT" })).toBeDefined();
    expect(`${router.state.location.pathname}${router.state.location.search}`).toBe(
      "/login?next=%2Fsettings%3Ftab%3Daccount",
    );
  });

  test("the account's language and theme apply to the pages and the document", async () => {
    // The browser's copy says English and light; the server's values win once they arrive.
    renderAt("/", {
      routes: ROUTES,
      identity: signedInBrowser({ preferences: { language: "zh-TW", theme: "dark" } }).browser,
      preferences: memoryPreferences({ language: "en", theme: "light" }),
    });
    expect(await screen.findByRole("heading", { name: "首頁" })).toBeDefined();
    expect(document.documentElement.lang).toBe("zh-TW");
    expect(document.documentElement.dataset.theme).toBe("dark");
  });

  test("signed out, the browser's copy of the preferences applies", async () => {
    renderAt("/login", {
      routes: ROUTES,
      identity: new FakeIdentity().browser(),
      preferences: memoryPreferences({ language: "zh-TW", theme: "dark" }),
    });
    expect(await screen.findByRole("heading", { name: "登入 TrustIoT" })).toBeDefined();
    expect(document.documentElement.lang).toBe("zh-TW");
    expect(document.documentElement.dataset.theme).toBe("dark");
  });
});

function failingRoutes(error: unknown): RouteObject[] {
  const Fail = () => {
    throw error;
  };
  return [
    {
      path: "/",
      element: <Layout />,
      errorElement: <ErrorPage />,
      children: [{ errorElement: <ErrorPage />, children: [{ path: "broken", element: <Fail /> }] }],
    },
  ];
}

describe("error pages", () => {
  const quiet = (run: () => Promise<void>) => async () => {
    const original = console.error;
    console.error = () => {};
    try {
      await run();
    } finally {
      console.error = original;
    }
  };

  test(
    "503 from the ingress shows the unavailable page, not sign-in",
    quiet(async () => {
      renderAt("/broken", { routes: failingRoutes(new ApiError(503, null)) });
      expect(await screen.findByRole("heading", { name: "Service temporarily unavailable" })).toBeDefined();
      expect(screen.getByRole("button", { name: "Reload" })).toBeDefined();
    }),
  );

  test(
    "401 offers to sign in again, returning to the page",
    quiet(async () => {
      renderAt("/broken", { routes: failingRoutes(new ApiError(401, null)) });
      const link = await screen.findByRole("link", { name: "Sign in again" });
      expect(link.getAttribute("href")).toBe("/login?next=%2Fbroken");
    }),
  );

  test(
    "403, a suspended tenant and other M1 errors",
    quiet(async () => {
      renderAt("/broken", { routes: failingRoutes(new ApiError(403, null)) });
      expect(await screen.findByRole("heading", { name: "No permission" })).toBeDefined();
      cleanup();
      renderAt("/broken", {
        routes: failingRoutes(new ApiError(403, { code: "tenant_suspended", message_key: "error.tenant_suspended" })),
      });
      expect(await screen.findByRole("heading", { name: "Organization suspended" })).toBeDefined();
      cleanup();
      renderAt("/broken", {
        routes: failingRoutes(new ApiError(409, { code: "conflict", message_key: "error.conflict" })),
      });
      expect(await screen.findByText("This was changed by someone else. Reload and try again.")).toBeDefined();
    }),
  );

  test(
    "a chunk that cannot be loaded shows the error page, not a blank screen",
    quiet(async () => {
      // What a lazily loaded page does when its chunk is gone and the reload did not happen.
      const routes: RouteObject[] = [
        {
          path: "/",
          element: <Layout />,
          HydrateFallback: PageLoading,
          errorElement: <ErrorPage />,
          children: [
            {
              errorElement: <ErrorPage />,
              children: [
                {
                  path: "later",
                  lazy: async () => {
                    throw new TypeError("Failed to fetch dynamically imported module");
                  },
                },
              ],
            },
          ],
        },
      ];
      renderAt("/later", { routes });
      expect(await screen.findByRole("heading", { name: "Something went wrong" })).toBeDefined();
      expect(screen.getByRole("button", { name: "Reload" })).toBeDefined();
    }),
  );

  test(
    "a bug shows a generic message and no details",
    quiet(async () => {
      renderAt("/broken", { routes: failingRoutes(new Error("internal detail: token=abc")) });
      expect(await screen.findByRole("heading", { name: "Something went wrong" })).toBeDefined();
      expect(screen.queryByText(/internal detail/)).toBeNull();
    }),
  );
});
