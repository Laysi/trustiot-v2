import { CircleAlertIcon, CloudOffIcon, CompassIcon, LockIcon } from "lucide-react";
import { Link, useLocation } from "react-router";

import { loginPath } from "../app/paths.ts";
import { Button } from "../components/ui/button.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { StatusPage } from "./StatusPage.tsx";

/** The home page; the operations overview replaces it (Laysi/trustiot-v2#129). */
export function HomePage() {
  const { t } = useI18n();
  return (
    <section className="grid gap-2">
      <h1 className="text-2xl font-semibold tracking-tight">{t("page.home.title")}</h1>
      <p className="text-muted-foreground">{t("page.home.body")}</p>
    </section>
  );
}

function HomeLink() {
  const { t } = useI18n();
  return (
    <Button asChild variant="outline">
      <Link to="/">{t("page.action.home")}</Link>
    </Button>
  );
}

function ReloadButton() {
  const { t } = useI18n();
  return <Button onClick={() => window.location.reload()}>{t("page.action.reload")}</Button>;
}

export function NotFoundPage() {
  const { t } = useI18n();
  const { pathname } = useLocation();
  return (
    <StatusPage title={t("page.not_found.title")} icon={<CompassIcon />} actions={<HomeLink />}>
      <p>{t("page.not_found.body", { path: pathname })}</p>
    </StatusPage>
  );
}

export function ForbiddenPage() {
  const { t } = useI18n();
  return (
    <StatusPage title={t("page.forbidden.title")} icon={<LockIcon />} actions={<HomeLink />}>
      <p>{t("page.forbidden.body")}</p>
    </StatusPage>
  );
}

/** The ingress answers 503 when the identity service cannot be reached: not a sign-in problem. */
export function UnavailablePage() {
  const { t } = useI18n();
  return (
    <StatusPage title={t("page.unavailable.title")} icon={<CloudOffIcon />} actions={<ReloadButton />}>
      <p>{t("page.unavailable.body")}</p>
    </StatusPage>
  );
}

/** Shown when the session ended while a page was open. */
export function SignedOutPage() {
  const { t } = useI18n();
  const location = useLocation();
  const target = loginPath(`${location.pathname}${location.search}${location.hash}`);
  return (
    <StatusPage
      title={t("error.unauthenticated")}
      icon={<LockIcon />}
      actions={
        <Button asChild>
          <Link to={target}>{t("page.action.sign_in")}</Link>
        </Button>
      }
    >
      <p>{t("page.login.body")}</p>
    </StatusPage>
  );
}

export function GenericErrorPage({ message }: { message?: string }) {
  const { t } = useI18n();
  return (
    <StatusPage title={t("page.error.title")} icon={<CircleAlertIcon />} actions={<ReloadButton />}>
      <p>{message ?? t("page.error.body")}</p>
    </StatusPage>
  );
}
