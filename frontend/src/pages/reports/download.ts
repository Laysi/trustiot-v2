import { createContext, useContext } from "react";

import type { DownloadFormat, ReportKind, ReportSummary } from "../../api/pending/reports.ts";

const DATE = /^\d{4}-\d{2}-\d{2}$/;
const KINDS: ReadonlySet<string> = new Set<ReportKind>(["weekly_energy", "monthly_carbon"]);

/**
 * The name the file is saved under. It is built from the report's type and the start of its
 * period only after both pass a check, never from the server's title or headers: a report's
 * fields are data, and a name with a path or a control character must not reach the file system.
 */
export function downloadName(report: Pick<ReportSummary, "kind" | "period_start">, format: DownloadFormat): string {
  const kind = KINDS.has(report.kind) ? report.kind.replaceAll("_", "-") : "report";
  const period = DATE.test(report.period_start) ? report.period_start : "unknown-period";
  const extension = format === "pdf" ? "pdf" : "csv";
  return `trustiot-${kind}-${period}.${extension}`;
}

/** What saves a file: the browser's (an anchor with `download` on an object URL), or a test's. */
export interface FileSaver {
  save(blob: Blob, name: string): void;
}

export const browserFileSaver: FileSaver = {
  save(blob, name) {
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = name;
    link.rel = "noopener";
    document.body.append(link);
    link.click();
    link.remove();
    // The click has started the download; the URL is released after the browser took the data.
    window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
  },
};

export const FileSaverContext = createContext<FileSaver>(browserFileSaver);

export function useFileSaver(): FileSaver {
  return useContext(FileSaverContext);
}
