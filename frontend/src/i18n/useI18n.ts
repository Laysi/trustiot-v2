import { useMemo } from "react";

import { ApiError } from "../api/errors.ts";
import { usePreferences } from "../preferences/PreferencesProvider.tsx";
import { translateError } from "./errors.ts";
import { formatDateTime, formatNumber } from "./format.ts";
import type { Locale, MessageId } from "./messages.ts";
import { type MessageValues, translate } from "./translate.ts";

export interface I18n {
  locale: Locale;
  /** The message in the user's language; only known keys type-check. */
  t(id: MessageId, values?: MessageValues): string;
  /** The text for any error a request or page threw. */
  errorMessage(error: unknown): string;
  formatDateTime(instant: Date | string): string;
  formatNumber(value: number): string;
}

export function useI18n(): I18n {
  const { preferences, timeZone } = usePreferences();
  const locale = preferences.locale;
  return useMemo<I18n>(
    () => ({
      locale,
      t: (id, values) => translate(locale, id, values),
      errorMessage: (error) =>
        error instanceof ApiError
          ? translateError(locale, error.body, error.status)
          : translate(locale, "app.error.unexpected"),
      formatDateTime: (instant) => formatDateTime(instant, locale, timeZone),
      formatNumber: (value) => formatNumber(value, locale),
    }),
    [locale, timeZone],
  );
}
