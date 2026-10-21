import { CATALOGS, type Locale, type MessageId } from "./messages.ts";

/** Values a message refers to as `{name}`. They are text, never markup: React escapes them. */
export type MessageValues = Readonly<Record<string, string | number>>;

/** `{name}` in a message; names use the same characters as message key segments. */
export const PLACEHOLDER = /\{([a-z0-9_]+)\}/g;

/** The placeholder names a message uses, sorted. */
export function placeholders(message: string): string[] {
  return [...new Set(Array.from(message.matchAll(PLACEHOLDER), (match) => match[1] as string))].sort();
}

/**
 * The message `id` in `locale` with its placeholders filled in. Numbers are formatted for the
 * locale. A placeholder without a value stays as it is, so a missing value shows up in review
 * instead of silently disappearing.
 */
export function translate(locale: Locale, id: MessageId, values?: MessageValues): string {
  const message = CATALOGS[locale][id];
  if (values === undefined) {
    return message;
  }
  const numbers = new Intl.NumberFormat(locale);
  return message.replace(PLACEHOLDER, (whole, name: string) => {
    const value = values[name];
    if (value === undefined) {
      return whole;
    }
    return typeof value === "number" ? numbers.format(value) : value;
  });
}
