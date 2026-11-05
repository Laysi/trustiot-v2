import { useMutation } from "@tanstack/react-query";

import { useReportApi } from "../../api/pending/ReportApiProvider.tsx";
import type { DownloadFormat, ReportSummary } from "../../api/pending/reports.ts";
import { downloadName, useFileSaver } from "./download.ts";

/** Fetches a report's file and saves it under a name the page builds (`downloadName`). */
export function useDownload() {
  const api = useReportApi();
  const saver = useFileSaver();
  return useMutation({
    mutationFn: async (request: {
      report: Pick<ReportSummary, "id" | "kind" | "period_start">;
      format: DownloadFormat;
    }) => {
      const blob = await api.download(request.report.id, request.format);
      saver.save(blob, downloadName(request.report, request.format));
    },
  });
}
