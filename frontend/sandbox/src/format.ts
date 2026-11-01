// How a value turns into text (frontend/sandbox/README.md, 「格式化」). `tq.format` and the
// comparison call the same function with the same bundled d3-format and d3-time-format, so the
// page and the check print the same string; Intl is not used, because its output depends on the
// browser's own data.
//
// A format may not add digits of its own: a number format with a digit as its fill character
// ("5>10" pads with fives) and a time format with digits outside its directives ("%Y (99)") are
// refused, because the comparison would accept whatever digits the format added.
import { formatLocale, formatSpecifier } from "d3-format";
import { timeFormatLocale } from "d3-time-format";
import { NUMBER_LOCALE_DEFINITIONS, TIME_LOCALE_DEFINITIONS } from "./locales.ts";
import type { Locale } from "./protocol.ts";

/** A value as a page sees it: a number, text, a boolean, a wall-clock `Date`, or `null`. */
export type DisplayValue = number | string | boolean | Date | null;

/** Longest format string, in characters. */
export const MAX_FORMAT_LEN = 64;

/** Widest padding a number format may ask for. */
export const MAX_FORMAT_WIDTH = 32;

/** How `null` reads everywhere: an em dash. */
export const NO_VALUE = "\u2014";

/** The number format when a page gives none. */
export const DEFAULT_NUMBER_FORMAT = ",.2~f";

const NUMBER_LOCALES = {
  "zh-TW": formatLocale(NUMBER_LOCALE_DEFINITIONS["zh-TW"]),
  en: formatLocale(NUMBER_LOCALE_DEFINITIONS.en),
} as const;

const TIME_LOCALES = {
  "zh-TW": timeFormatLocale(TIME_LOCALE_DEFINITIONS["zh-TW"]),
  en: timeFormatLocale(TIME_LOCALE_DEFINITIONS.en),
} as const;

const BOOLEANS = {
  "zh-TW": ["是", "否"],
  en: ["Yes", "No"],
} as const;

/** The d3-time-format directives a time format may use. `%Z` is left out: it prints a fixed "+0000". */
const TIME_DIRECTIVES = "aAbBcdefgGHIjLmMpqQsSuUVwWxXyY%";
const DIGIT = /\p{Nd}/u;

/**
 * The d3-format types (d3-format 3.1.2's formatTypes, plus `n` and none). d3 reads any other
 * letter as `g`; here it is not a format.
 */
const NUMBER_TYPES = new Set(["", "%", "b", "c", "d", "e", "f", "g", "n", "o", "p", "r", "s", "x", "X"]);

/** A format string a value cannot be formatted with. */
export class FormatError extends Error {
  override readonly name = "FormatError";
}

type Formatter<T> = (value: T) => string;
const numberFormatters: Record<string, Formatter<number>> = Object.create(null);
const timeFormatters: Record<string, Formatter<Date>> = Object.create(null);
let cached = 0;

function remember<T>(store: Record<string, Formatter<T>>, key: string, make: () => Formatter<T>): Formatter<T> {
  const found = store[key];
  if (found !== undefined) return found;
  const formatter = make();
  // A page cannot grow the cache without bound; past the limit, formatters are rebuilt.
  if (cached < 256) {
    store[key] = formatter;
    cached++;
  }
  return formatter;
}

function numberFormatter(format: string, locale: Locale): Formatter<number> {
  return remember(numberFormatters, `${locale}\u0000${format}`, () => {
    if (format.length > MAX_FORMAT_LEN) throw new FormatError("the number format is too long");
    let specifier: ReturnType<typeof formatSpecifier>;
    try {
      specifier = formatSpecifier(format);
    } catch {
      throw new FormatError(`not a d3-format specifier: ${format}`);
    }
    if (!NUMBER_TYPES.has(specifier.type)) throw new FormatError(`not a d3-format type: ${specifier.type}`);
    const zeroPadding = specifier.fill === "0" && specifier.align === "=";
    if (DIGIT.test(specifier.fill) && !zeroPadding) {
      throw new FormatError("a number format may not pad with digits; use the 0 flag");
    }
    if (specifier.width !== undefined && specifier.width > MAX_FORMAT_WIDTH) {
      throw new FormatError(`a number format may pad to at most ${MAX_FORMAT_WIDTH} characters`);
    }
    return NUMBER_LOCALES[locale].format(format);
  });
}

function timeFormatter(format: string, locale: Locale): Formatter<Date> {
  return remember(timeFormatters, `${locale}\u0000${format}`, () => {
    if (format.length > MAX_FORMAT_LEN) throw new FormatError("the time format is too long");
    for (let i = 0; i < format.length; i++) {
      const c = format.charAt(i);
      if (c === "%") {
        let directive = format.charAt(++i);
        if (directive === "-" || directive === "_" || directive === "0") directive = format.charAt(++i);
        if (directive === "" || !TIME_DIRECTIVES.includes(directive)) {
          throw new FormatError(`not a d3-time-format directive: %${directive}`);
        }
      } else if (DIGIT.test(c)) {
        throw new FormatError("a time format may not write digits of its own");
      }
    }
    return TIME_LOCALES[locale].utcFormat(format);
  });
}

/** The time format when a page gives none: the date alone at midnight, else date, hour and minute. */
function defaultTimeFormat(date: Date): string {
  const midnight =
    date.getUTCHours() === 0 &&
    date.getUTCMinutes() === 0 &&
    date.getUTCSeconds() === 0 &&
    date.getUTCMilliseconds() === 0;
  return midnight ? "%Y-%m-%d" : "%Y-%m-%d %H:%M";
}

/**
 * Formats a value (`tq.format`): `null` as 「—」, a number with a d3-format specifier, a
 * wall-clock `Date` with a d3-time-format `utcFormat` specifier, text as it is, a boolean as the
 * locale's yes or no. `format` applies to numbers and times only.
 *
 * @throws FormatError when `format` is malformed or would add digits of its own.
 */
export function formatValue(value: DisplayValue, format: string | undefined, locale: Locale): string {
  if (value === null) return NO_VALUE;
  if (typeof value === "number") return numberFormatter(format ?? DEFAULT_NUMBER_FORMAT, locale)(value);
  if (typeof value === "string") return value;
  if (typeof value === "boolean") return BOOLEANS[locale][value ? 0 : 1];
  if (!Number.isFinite(value.getTime())) return NO_VALUE;
  return timeFormatter(format ?? defaultTimeFormat(value), locale)(value);
}
