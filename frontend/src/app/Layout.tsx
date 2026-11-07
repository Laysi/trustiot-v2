import { XIcon } from "lucide-react";
import { Navigate, Outlet, ScrollRestoration, useLocation } from "react-router";

import { Alert, AlertDescription } from "../components/ui/alert.tsx";
import { Button, focusRing } from "../components/ui/button.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { cn } from "../lib/utils.ts";
import { StatusPage } from "../pages/StatusPage.tsx";
import { PinSession } from "../session/PinSession.tsx";
import { useSession } from "../session/SessionProvider.tsx";
import { tenantName } from "../session/tenant-name.ts";
import { useSignOut } from "../session/useSignOut.ts";
import { AppHeader } from "./AppHeader.tsx";
import { HoldBackScroll } from "./HoldBackScroll.tsx";
import { PageLoading } from "./PageLoading.tsx";
import { loginPath } from "./paths.ts";

/** The product's mark and name, on the sign-in page; the mark is the favicon, so it is one file. */
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
 * The frame of every signed-in page: the top bar (AppHeader) and the page below it. A visitor
 * without a session is sent to sign in, and comes back here afterwards.
 *
 * It also renders the router's `ScrollRestoration`, so every page opened by a link starts at the
 * top while the browser's back and forward buttons return to the earlier position, and a `#hash`
 * still scrolls to its target (Laysi/trustiot-v2#1185); `HoldBackScroll` keeps a restored position
 * while the page's content arrives. The sign-in page, the one route outside
 * this layout, renders its own.
 */
export function Layout() {
  return (
    <>
      <ScrollRestoration />
      <HoldBackScroll />
      <LayoutFrame />
    </>
  );
}

function LayoutFrame() {
  const { t, errorMessage } = useI18n();
  const { status, session, error, reload, notice, dismissNotice } = useSession();
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
      <AppHeader onSignOut={() => signOut.mutate("this")} signingOut={signOut.isPending} />
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
      {notice ? (
        <div className="mx-auto w-full max-w-6xl px-4 pt-4">
          <Alert role="status" className="grid-cols-[1fr_auto] items-center">
            <AlertDescription className="col-start-1 text-foreground">
              {notice.kind === "tenant_changed"
                ? t("session_sync.tenant_changed", { name: tenantName(notice.tenant, t) })
                : t(notice.kind === "tenant_left" ? "session_sync.tenant_left" : "session_sync.account_changed")}
            </AlertDescription>
            <Button variant="ghost" size="icon" aria-label={t("session_sync.dismiss")} onClick={dismissNotice}>
              <XIcon />
            </Button>
          </Alert>
        </div>
      ) : null}
      <main id="main" tabIndex={-1} className="mx-auto w-full max-w-6xl flex-1 px-4 py-8 outline-none">
        {/* The pages belong to one user: signed in as someone else in another tab, they start
            afresh, and nothing typed into them for the previous user is kept; every request they
            make names the user they were mounted for (PinSession). */}
        <PinSession key={session?.user.id} pin={session ? { user: session.user.id } : {}}>
          <Outlet />
        </PinSession>
      </main>
    </div>
  );
}
