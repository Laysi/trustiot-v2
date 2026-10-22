import type { RouteObject } from "react-router";

import { ErrorPage } from "../pages/ErrorPage.tsx";
import { HomePage, LoginPage, NotFoundPage } from "../pages/pages.tsx";
import { Layout } from "./Layout.tsx";
import { LOGIN_PATH } from "./paths.ts";

/**
 * Every client route. Each page of MS2 adds its route here; routes.test.ts keeps them out of the
 * paths the ingress does not send to the SPA (paths.ts) and checks that deep links render.
 * Every route has an error boundary, so a failing page never leaves a blank screen.
 */
export const ROUTES: RouteObject[] = [
  {
    path: LOGIN_PATH,
    element: <LoginPage />,
    errorElement: <ErrorPage />,
  },
  {
    path: "/",
    element: <Layout />,
    errorElement: <ErrorPage />,
    children: [
      {
        errorElement: <ErrorPage />,
        children: [
          { index: true, element: <HomePage /> },
          { path: "*", element: <NotFoundPage /> },
        ],
      },
    ],
  },
];
