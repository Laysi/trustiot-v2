import { useMemo } from "react";

import { ApiError } from "../api/errors.ts";
import { usePreferences } from "../preferences/PreferencesProvider.tsx";
import { translateError, translateFieldError } from "./errors.ts";
import { formatDateTime, formatNumber } from "./format.ts";
import type { Locale, MessageId } from "./messages.ts";
import { type MessageValues, translate } from "./translate.ts";

export interface I18n {
  locale: Locale;
  /** The message in the user's language; only known keys type-check. */
  t(id: MessageId, values?: MessageValues): string;
  /** The text for any error a request or page threw. */
  errorMessage(error: unknown): string;
  /**
   * The text for the field error an API error names at `pointer` (`/email`), if it names one.
   * Pages keep the error, not the text, so a change of language translates it again.
   */
  fieldError(error: unknown, pointer: string): string | undefined;
  formatDateTime(instant: Date | string): string;
  formatNumber(value: number): string;
}

export function useI18n(): I18n {
  const { locale, timeZone } = usePreferences();
  return useMemo<I18n>(
    () => ({
      locale,
      t: (id, values) => translate(locale, id, values),
      errorMessage: (error) =>
        error instanceof ApiError
          ? translateError(locale, error.body, error.status)
          : translate(locale, "app.error.unexpected"),
      fieldError: (error, pointer) => {
        if (!(error instanceof ApiError) || error.body === null) return undefined;
        const field = error.body.field_errors?.find((item) => item.field === pointer);
        return field ? translateFieldError(locale, field, error.body.code) : undefined;
      },
      formatDateTime: (instant) => formatDateTime(instant, locale, timeZone),
      formatNumber: (value) => formatNumber(value, locale),
    }),
    [locale, timeZone],
  );
}
