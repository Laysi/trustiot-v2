// The translations. Keys use the server's message key format (crates/contract, `MessageKey`), so a
// `message_key` from an error response is looked up as it is. en.json defines the key set: a key
// used in code but missing there does not type-check, and i18n.test.ts fails when the two files
// differ in keys or placeholders.
import en from "./locales/en.json";
import zhTW from "./locales/zh-TW.json";

export type MessageId = keyof typeof en;

export const LOCALES = ["zh-TW", "en"] as const;
export type Locale = (typeof LOCALES)[number];

/** Every locale's catalog has every key; the annotation makes a key missing from one a type error. */
export const CATALOGS: Readonly<Record<Locale, Readonly<Record<MessageId, string>>>> = {
  "zh-TW": zhTW,
  en,
};

/** The server's message key format (`MessageKey` in crates/contract/src/error.rs). */
export const MESSAGE_KEY_FORMAT = /^[a-z0-9_]+(\.[a-z0-9_]+)+$/;

const KNOWN: ReadonlySet<string> = new Set(Object.keys(en));

export function isMessageId(key: string): key is MessageId {
  return KNOWN.has(key);
}

export function isLocale(value: unknown): value is Locale {
  return typeof value === "string" && (LOCALES as readonly string[]).includes(value);
}
