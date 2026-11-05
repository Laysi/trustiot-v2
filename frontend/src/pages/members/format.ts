import { usePreferences } from "../../preferences/PreferencesProvider.tsx";

/** A date as the design draws it (2026-11-08), in the time zone of the user's preferences. */
export function useDateFormat(): (instant: string) => string {
  const { timeZone } = usePreferences();
  // `sv-SE` is year-month-day with dashes whatever the page's language.
  const format = new Intl.DateTimeFormat("sv-SE", { year: "numeric", month: "2-digit", day: "2-digit", timeZone });
  return (instant) => format.format(new Date(instant));
}
