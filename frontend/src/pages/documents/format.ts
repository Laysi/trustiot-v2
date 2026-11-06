import type { Locale } from "../../i18n/messages.ts";

const UNITS = ["B", "KB", "MB", "GB", "TB"] as const;

/** A size as the design writes it (`1.4 GB`): decimal units, one decimal at most. */
export function formatBytes(bytes: number, locale: Locale): string {
  let value = Math.max(0, bytes);
  let unit = 0;
  while (value >= 1000 && unit < UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  const digits = unit === 0 ? 0 : 1;
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: digits }).format(value)} ${UNITS[unit]}`;
}

/** Whether a picked file may go to the library: the library takes PDFs only. */
export function isPdf(file: File): boolean {
  return file.type === "application/pdf" || /\.pdf$/i.test(file.name);
}

/** The name a document gets from a file: the file's name without `.pdf`. */
export function documentName(file: File): string {
  return file.name.replace(/\.pdf$/i, "");
}
