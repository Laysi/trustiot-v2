import { NavLink, Outlet } from "react-router";

import { useI18n } from "../i18n/useI18n.ts";
import { PreferencesMenu } from "../preferences/PreferencesMenu.tsx";
import { TenantSwitcher } from "../session/TenantSwitcher.tsx";

/** The frame of every signed-in page: the header with navigation, tenant and preferences. */
export function Layout() {
  const { t } = useI18n();
  return (
    <div className="layout">
      <header className="layout__header">
        <NavLink to="/" className="layout__brand">
          {t("app.name")}
        </NavLink>
        <nav aria-label={t("app.main_navigation")} className="layout__nav">
          <NavLink to="/" end>
            {t("nav.home")}
          </NavLink>
        </nav>
        <div className="layout__tools">
          <TenantSwitcher />
          <PreferencesMenu />
        </div>
      </header>
      <main id="main" className="layout__main">
        <Outlet />
      </main>
    </div>
  );
}
