import { useId, useState } from "react";

import { useI18n } from "../i18n/useI18n.ts";
import { useSession } from "./SessionProvider.tsx";

/** The active tenant's name, and a choice of the user's other tenants when there are any. */
export function TenantSwitcher() {
  const { t } = useI18n();
  const { session, switchTenant } = useSession();
  const [failed, setFailed] = useState(false);
  const [switching, setSwitching] = useState(false);
  const id = useId();

  if (session === null) {
    return null;
  }
  const active = session.activeTenant;
  if (session.tenants.length <= 1) {
    return (
      <div className="tenant-switcher">
        <span className="tenant-switcher__label">{t("tenant_switcher.label")}</span>{" "}
        <span className="tenant-switcher__name">{active?.name ?? t("tenant_switcher.none")}</span>
      </div>
    );
  }
  return (
    <div className="tenant-switcher">
      <label htmlFor={id} className="tenant-switcher__label">
        {t("tenant_switcher.label")}
      </label>{" "}
      <select
        id={id}
        value={active?.id ?? ""}
        disabled={switching}
        onChange={async (event) => {
          setFailed(false);
          setSwitching(true);
          try {
            await switchTenant(event.target.value);
          } catch {
            setFailed(true);
          } finally {
            setSwitching(false);
          }
        }}
      >
        {active === null ? <option value="">{t("tenant_switcher.none")}</option> : null}
        {session.tenants.map((tenant) => (
          <option key={tenant.id} value={tenant.id}>
            {tenant.name}
          </option>
        ))}
      </select>
      {failed ? (
        <p role="alert" className="tenant-switcher__error">
          {t("tenant_switcher.switch_failed")}
        </p>
      ) : null}
    </div>
  );
}
