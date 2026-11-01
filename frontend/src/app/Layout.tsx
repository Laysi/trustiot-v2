import { XIcon } from "lucide-react";
import { Link, Navigate, NavLink, Outlet, useLocation } from "react-router";

import { Alert, AlertDescription } from "../components/ui/alert.tsx";
import { Button, focusRing } from "../components/ui/button.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { cn } from "../lib/utils.ts";
import { StatusPage } from "../pages/StatusPage.tsx";
import { useSession } from "../session/SessionProvider.tsx";
import { TenantSwitcher } from "../session/TenantSwitcher.tsx";
import { useSignOut } from "../session/useSignOut.ts";
import { PageLoading } from "./PageLoading.tsx";
import { loginPath } from "./paths.ts";
import { UserMenu } from "./UserMenu.tsx";

/** The product's mark and name; the mark is the favicon, so it is one file. */
export function Brand({ className }: { className?: string }) {
  const { t } = useI18n();
  return (
    <span className={cn("flex items-center gap-2 font-semibold", className)}>
      <img src="/favicon.svg" alt="" className="size-7" />
      {t("app.name")}
    </span>
  );
}

/**
 * The frame of every signed-in page: the top bar with navigation, the tenant and the account
 * menu. A visitor without a session is sent to sign in, and comes back here afterwards.
 */
export function Layout() {
  const { t, errorMessage } = useI18n();
  const { status, error, reload } = useSession();
  const location = useLocation();
  const signOut = useSignOut();

  if (status === "loading") {
    return <PageLoading />;
  }
  if (status === "error") {
    return (
      <main className="px-4">
        <StatusPage
          title={t("page.unavailable.title")}
          actions={<Button onClick={reload}>{t("page.action.retry")}</Button>}
        >
          <p>{errorMessage(error)}</p>
        </StatusPage>
      </main>
    );
  }
  if (status === "signed_out") {
    return <Navigate to={loginPath(`${location.pathname}${location.search}${location.hash}`)} replace />;
  }

  return (
    <div className="flex min-h-dvh flex-col">
      <a
        href="#main"
        className={cn(
          "sr-only rounded-md bg-background px-3 py-2 text-sm font-medium focus:not-sr-only focus:fixed focus:top-2 focus:left-2 focus:z-50",
          focusRing,
        )}
      >
        {t("app.skip_to_content")}
      </a>
      <header className="sticky top-0 z-40 border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/80">
        <div className="mx-auto flex min-h-14 max-w-6xl flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2 sm:gap-x-6">
          <Link to="/" className={cn("rounded-md", focusRing)}>
            <Brand />
          </Link>
          <nav aria-label={t("app.main_navigation")} className="flex items-center gap-1 text-sm">
            <NavLink
              to="/"
              end
              className={({ isActive }) =>
                cn(
                  "rounded-md px-3 py-1.5 font-medium text-muted-foreground transition-colors hover:text-foreground",
                  isActive && "bg-accent text-accent-foreground",
                  focusRing,
                )
              }
            >
              {t("nav.home")}
            </NavLink>
          </nav>
          <div className="ml-auto flex min-w-0 items-center gap-2">
            <TenantSwitcher />
            <UserMenu onSignOut={() => signOut.mutate("this")} signingOut={signOut.isPending} />
          </div>
        </div>
      </header>
      {signOut.isError ? (
        <div className="mx-auto w-full max-w-6xl px-4 pt-4">
          <Alert variant="destructive" className="grid-cols-[1fr_auto] items-center">
            <AlertDescription className="col-start-1">
              {t("user_menu.sign_out_failed", { reason: errorMessage(signOut.error) })}
            </AlertDescription>
            <Button variant="ghost" size="icon" aria-label={t("user_menu.dismiss")} onClick={() => signOut.reset()}>
              <XIcon />
            </Button>
          </Alert>
        </div>
      ) : null}
      <main id="main" tabIndex={-1} className="mx-auto w-full max-w-6xl flex-1 px-4 py-8 outline-none">
        <Outlet />
      </main>
    </div>
  );
}
