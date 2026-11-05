import type { ProviderReconciliation } from "../../../api/pending/aiUsage.ts";

/** `invoice - ledger` and its share of the ledger, or `null` while either amount is missing. */
export function difference(row: ProviderReconciliation): { usd: number; percent: number } | null {
  if (row.ledger_usd === null || row.invoice_usd === null || row.ledger_usd === 0) return null;
  const usd = row.invoice_usd - row.ledger_usd;
  return { usd, percent: (usd / row.ledger_usd) * 100 };
}

/** A CSV field: quoted when it holds a comma, quote or line break, and never read as a formula by a spreadsheet. */
function field(value: string): string {
  const safe = /^[=+\-@\t\r]/.test(value) ? `'${value}` : value;
  return /[",\r\n]/.test(safe) ? `"${safe.replaceAll('"', '""')}"` : safe;
}

/** The reconciliation of a month as CSV: one line per provider, amounts in US dollars. */
export function reconciliationCsv(month: string, rows: readonly ProviderReconciliation[]): string {
  const lines = [
    ["month", "provider", "ledger_usd", "invoice_usd", "difference_usd", "difference_percent", "status"].join(","),
  ];
  for (const row of rows) {
    const diff = difference(row);
    lines.push(
      [
        field(month),
        field(row.provider),
        row.ledger_usd === null ? "" : row.ledger_usd.toFixed(2),
        row.invoice_usd === null ? "" : row.invoice_usd.toFixed(2),
        diff === null ? "" : diff.usd.toFixed(2),
        diff === null ? "" : diff.percent.toFixed(2),
        row.status,
      ].join(","),
    );
  }
  return `${lines.join("\r\n")}\r\n`;
}
