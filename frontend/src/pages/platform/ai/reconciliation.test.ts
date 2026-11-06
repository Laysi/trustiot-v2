import { describe, expect, test } from "bun:test";

import { difference, reconciliationCsv } from "./reconciliation.ts";

describe("the reconciliation's difference and CSV", () => {
  test("the difference is the invoice less the ledger, and its share of the ledger", () => {
    const diff = difference({ provider: "A", ledger_usd: 100, invoice_usd: 102.5, status: "within_tolerance" });
    expect(diff?.usd).toBeCloseTo(2.5);
    expect(diff?.percent).toBeCloseTo(2.5);
    expect(difference({ provider: "A", ledger_usd: null, invoice_usd: 5, status: "investigate" })).toBeNull();
    expect(difference({ provider: "A", ledger_usd: 5, invoice_usd: null, status: "awaiting_invoice" })).toBeNull();
  });

  test("one line per provider, empty fields where an amount is missing", () => {
    const csv = reconciliationCsv("2026-10", [
      { provider: "Anthropic", ledger_usd: 1204.55, invoice_usd: 1206.02, status: "within_tolerance" },
      { provider: "Vertex", ledger_usd: null, invoice_usd: null, status: "awaiting_invoice" },
    ]);
    expect(csv.split("\r\n")).toEqual([
      "month,provider,ledger_usd,invoice_usd,difference_usd,difference_percent,status",
      "2026-10,Anthropic,1204.55,1206.02,1.47,0.12,within_tolerance",
      "2026-10,Vertex,,,,,awaiting_invoice",
      "",
    ]);
  });

  test("a provider name is quoted when needed and never starts a spreadsheet formula", () => {
    const csv = reconciliationCsv("2026-10", [
      { provider: '=HYPERLINK("x")', ledger_usd: 1, invoice_usd: 1, status: "within_tolerance" },
      { provider: "A, B", ledger_usd: 1, invoice_usd: 1, status: "within_tolerance" },
    ]);
    const lines = csv.split("\r\n");
    expect(lines[1]).toStartWith(`2026-10,"'=HYPERLINK(""x"")",`);
    expect(lines[2]).toStartWith('2026-10,"A, B",');
  });
});
