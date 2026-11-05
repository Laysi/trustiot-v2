import { describe, expect, test } from "bun:test";

import type { ReportSummary } from "../../api/pending/reports.ts";
import { downloadName } from "./download.ts";

describe("the name a downloaded report is saved under", () => {
  test("is built from the type and the start of the period", () => {
    expect(downloadName({ kind: "monthly_carbon", period_start: "2026-10-01" }, "pdf")).toBe(
      "trustiot-monthly-carbon-2026-10-01.pdf",
    );
    expect(downloadName({ kind: "weekly_energy", period_start: "2026-10-26" }, "csv")).toBe(
      "trustiot-weekly-energy-2026-10-26.csv",
    );
  });

  test("never carries what the server sent when it is not a date or a known type", () => {
    for (const start of ["../../etc/passwd", "2026-10-01/../x", "a\u0000b", "2026-10-01.exe", "", "<script>"]) {
      const name = downloadName({ kind: "weekly_energy", period_start: start }, "csv");
      expect(name).toBe("trustiot-weekly-energy-unknown-period.csv");
    }
    const unknown = { kind: "../x" as unknown as ReportSummary["kind"], period_start: "2026-10-01" };
    expect(downloadName(unknown, "pdf")).toBe("trustiot-report-2026-10-01.pdf");
    // Only the two extensions the page offers.
    expect(downloadName({ kind: "weekly_energy", period_start: "2026-10-01" }, "exe" as unknown as "pdf")).toMatch(
      /\.csv$/,
    );
  });
});
