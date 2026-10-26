import type { MessageId } from "../i18n/messages.ts";
import { useI18n } from "../i18n/useI18n.ts";
import { usePreferences } from "./PreferencesProvider.tsx";
import { LANGUAGES, type LanguagePreference, THEMES, type Theme } from "./preferences.ts";

// Languages are named in their own language, so a user can find theirs whatever the page shows.
const LOCALE_NAMES = { "zh-TW": "settings.language.zh_tw", en: "settings.language.en" } as const satisfies Record<
  Exclude<LanguagePreference, "auto">,
  MessageId
>;

const THEME_LABELS = {
  system: "settings.theme.system",
  light: "settings.theme.light",
  dark: "settings.theme.dark",
} as const satisfies Record<Theme, MessageId>;

/** A value from the select's change event, if it is one of `values`. */
export function pick<T extends string>(values: readonly T[], value: string): T | undefined {
  return values.find((item) => item === value);
}

/** The interface languages; "auto" names the language it resolves to in this browser. */
export function LanguageOptions() {
  const { t } = useI18n();
  const { browserLocale } = usePreferences();
  return LANGUAGES.map((language) => (
    <option key={language} value={language}>
      {language === "auto"
        ? t("settings.language.auto", { language: t(LOCALE_NAMES[browserLocale]) })
        : t(LOCALE_NAMES[language])}
    </option>
  ));
}

/** The languages AI answers can be written in; "auto" answers in the question's language. */
export function AiLanguageOptions() {
  const { t } = useI18n();
  return LANGUAGES.map((language) => (
    <option key={language} value={language}>
      {language === "auto" ? t("settings.ai_language.auto") : t(LOCALE_NAMES[language])}
    </option>
  ));
}

export function ThemeOptions() {
  const { t } = useI18n();
  return THEMES.map((theme) => (
    <option key={theme} value={theme}>
      {t(THEME_LABELS[theme])}
    </option>
  ));
}
