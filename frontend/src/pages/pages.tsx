import { Link, useLocation } from "react-router";

import { loginPath } from "../app/paths.ts";
import { useI18n } from "../i18n/useI18n.ts";
import { StatusPage } from "./StatusPage.tsx";

/** The home page; the operations overview replaces it (Laysi/trustiot-v2#129). */
export function HomePage() {
  const { t } = useI18n();
  return (
    <section>
      <h1>{t("page.home.title")}</h1>
      <p>{t("page.home.body")}</p>
    </section>
  );
}

/** The sign-in page; the form comes with Laysi/trustiot-v2#124. */
export function LoginPage() {
  const { t } = useI18n();
  return (
    <StatusPage title={t("page.login.title")}>
      <p>{t("page.login.body")}</p>
    </StatusPage>
  );
}

function HomeLink() {
  const { t } = useI18n();
  return <Link to="/">{t("page.action.home")}</Link>;
}

function ReloadButton() {
  const { t } = useI18n();
  return (
    <button type="button" onClick={() => window.location.reload()}>
      {t("page.action.reload")}
    </button>
  );
}

export function NotFoundPage() {
  const { t } = useI18n();
  const { pathname } = useLocation();
  return (
    <StatusPage title={t("page.not_found.title")} actions={<HomeLink />}>
      <p>{t("page.not_found.body", { path: pathname })}</p>
    </StatusPage>
  );
}

export function ForbiddenPage() {
  const { t } = useI18n();
  return (
    <StatusPage title={t("page.forbidden.title")} actions={<HomeLink />}>
      <p>{t("page.forbidden.body")}</p>
    </StatusPage>
  );
}

export function TenantSuspendedPage() {
  const { t } = useI18n();
  return (
    <StatusPage title={t("page.tenant_suspended.title")}>
      <p>{t("page.tenant_suspended.body")}</p>
    </StatusPage>
  );
}

/** The ingress answers 503 when the identity service cannot be reached: not a sign-in problem. */
export function UnavailablePage() {
  const { t } = useI18n();
  return (
    <StatusPage title={t("page.unavailable.title")} actions={<ReloadButton />}>
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
    <StatusPage title={t("error.unauthenticated")} actions={<Link to={target}>{t("page.action.sign_in")}</Link>}>
      <p>{t("page.login.body")}</p>
    </StatusPage>
  );
}

export function GenericErrorPage({ message }: { message?: string }) {
  const { t } = useI18n();
  return (
    <StatusPage title={t("page.error.title")} actions={<ReloadButton />}>
      <p>{message ?? t("page.error.body")}</p>
    </StatusPage>
  );
}
