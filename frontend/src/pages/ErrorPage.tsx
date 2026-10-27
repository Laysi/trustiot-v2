import { useEffect } from "react";
import { isRouteErrorResponse, useRouteError } from "react-router";

import { ApiError, classifyError } from "../api/errors.ts";
import { useI18n } from "../i18n/useI18n.ts";
import { TenantSuspendedPage } from "../session/TenantSuspendedPage.tsx";
import { ForbiddenPage, GenericErrorPage, NotFoundPage, SignedOutPage, UnavailablePage } from "./pages.tsx";

/**
 * The routes' error boundary. API errors get the page that tells the user what to do; anything
 * else gets a generic message. Details stay in the console, never on the page.
 */
export function ErrorPage() {
  const error = useRouteError();
  const { errorMessage } = useI18n();
  useEffect(() => {
    console.error("route error", error);
  }, [error]);

  if (isRouteErrorResponse(error) && error.status === 404) {
    return <NotFoundPage />;
  }
  switch (classifyError(error)) {
    case "unauthenticated":
      return <SignedOutPage />;
    case "forbidden":
      return <ForbiddenPage />;
    case "tenant_suspended":
      return <TenantSuspendedPage />;
    case "unavailable":
      return <UnavailablePage />;
    case "not_found":
      return <NotFoundPage />;
    default:
      // An M1 error the user can act on (a conflict, a quota) says so; anything else stays generic.
      return <GenericErrorPage message={error instanceof ApiError ? errorMessage(error) : undefined} />;
  }
}
