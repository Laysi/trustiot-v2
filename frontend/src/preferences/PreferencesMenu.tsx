import { useId, useMemo } from "react";
import { isLocale } from "../i18n/messages.ts";
import { useI18n } from "../i18n/useI18n.ts";
import { usePreferences } from "./PreferencesProvider.tsx";
import { THEMES, type Theme } from "./preferences.ts";

const THEME_LABELS = {
  system: "preferences.theme.system",
  light: "preferences.theme.light",
  dark: "preferences.theme.dark",
} as const satisfies Record<Theme, string>;

function timeZones(): readonly string[] {
  try {
    return Intl.supportedValuesOf("timeZone");
  } catch {
    return [];
  }
}

/** Language, time zone and theme; changes apply at once and are kept in the preferences store. */
export function PreferencesMenu() {
  const { t } = useI18n();
  const { preferences, update, browserZone } = usePreferences();
  const zones = useMemo(() => {
    const all = timeZones();
    // A zone kept from another browser stays selectable even if this one does not list it.
    return preferences.timeZone !== "auto" && !all.includes(preferences.timeZone)
      ? [preferences.timeZone, ...all]
      : all;
  }, [preferences.timeZone]);
  const ids = { locale: useId(), zone: useId(), theme: useId() };

  return (
    <details className="preferences">
      <summary>{t("preferences.title")}</summary>
      <div className="preferences__fields">
        <label htmlFor={ids.locale}>{t("preferences.language")}</label>
        <select
          id={ids.locale}
          value={preferences.locale}
          onChange={(event) => {
            if (isLocale(event.target.value)) update({ locale: event.target.value });
          }}
        >
          <option value="zh-TW">{t("preferences.language.zh_tw")}</option>
          <option value="en">{t("preferences.language.en")}</option>
        </select>

        <label htmlFor={ids.zone}>{t("preferences.time_zone")}</label>
        <select
          id={ids.zone}
          value={preferences.timeZone}
          onChange={(event) => update({ timeZone: event.target.value })}
        >
          <option value="auto">{t("preferences.time_zone.auto", { zone: browserZone })}</option>
          {zones.map((zone) => (
            <option key={zone} value={zone}>
              {zone}
            </option>
          ))}
        </select>

        <label htmlFor={ids.theme}>{t("preferences.theme")}</label>
        <select
          id={ids.theme}
          value={preferences.theme}
          onChange={(event) => {
            const theme = THEMES.find((value) => value === event.target.value);
            if (theme) update({ theme });
          }}
        >
          {THEMES.map((theme) => (
            <option key={theme} value={theme}>
              {t(THEME_LABELS[theme])}
            </option>
          ))}
        </select>
      </div>
    </details>
  );
}
