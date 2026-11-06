import type { ComponentType, ReactNode } from "react";
import { createElement } from "react";

import { ApiError } from "../api/errors.ts";
import { AiUsageApiProvider } from "../api/pending/AiUsageApiProvider.tsx";
import type {
  AiUsageApi,
  CategoryUsage,
  ProviderReconciliation,
  Reconciliation,
  TenantUsage,
  UsageMonth,
  UsageSummary,
} from "../api/pending/aiUsage.ts";

/**
 * The AI usage and reconciliation API (Laysi/trustiot-v2#140, not built yet) in memory: a month is
 * its tenants and categories, the totals are summed from them, the reconciliation is only for a
 * month that is over (409 `conflict` while it is in progress), and a provider's invoice amount is
 * typed in by the administrator; its status follows a 2 percent tolerance here (the real
 * tolerance is the server's rule). `failNext` makes the next call to a method fail.
 */

export type UsageMethod = keyof AiUsageApi;

export interface FakeMonth {
  month: string;
  in_progress: boolean;
  as_of: string;
  tenants: TenantUsage[];
  categories: CategoryUsage[];
  calls: number;
  failed_calls: number;
  refused_calls: number;
  /** The ledger's amount per provider, and the invoices typed in so far. */
  ledger: Record<string, number>;
  invoices: Record<string, number>;
}

const TOLERANCE = 0.02;

export class FakeAiUsageServer {
  readonly months = new Map<string, FakeMonth>();
  readonly calls: string[] = [];
  private readonly failures = new Map<UsageMethod, ApiError[]>();

  constructor(options: { seed?: "normal" | "empty" } = {}) {
    if ((options.seed ?? "normal") === "empty") {
      this.months.set("2026-11", emptyMonth("2026-11", true));
      return;
    }
    this.months.set("2026-11", {
      month: "2026-11",
      in_progress: true,
      as_of: "2026-11-01T02:05:00Z",
      calls: 18240,
      failed_calls: 212,
      refused_calls: 9,
      tenants: [
        tenant("t-acme-labs", "Acme Labs Test", 10.31, 10, {
          unmetered: 0.42,
          estimated: 2,
          overdrawn: 0.31,
          blockedSince: "2026-11-01T02:01:00Z",
        }),
        tenant(
          "t-second-plant",
          "Second Plant Manufacturing Co., a very long tenant name used to test wrapping",
          41.72,
          50,
          { unmetered: 1.1 },
        ),
        tenant("t-north", "North Demo Factory", 88.1, 300, { unmetered: 3.05, estimated: 31, lowerBound: 4 }),
        tenant("t-demo-a", "Demo Tenant A", 12.4, 70, { raise: 20, unmetered: 0.2, estimated: 4 }),
        {
          tenant_id: null,
          name: null,
          amount_usd: 14.08,
          quota_usd: null,
          quota_raise_usd: 0,
          unmetered_usd: null,
          estimated_entries: 0,
          lower_bound_entries: 0,
          overdrawn_usd: 0,
          blocked_since: null,
        },
      ],
      categories: [
        cat("anthropic/claude-opus-5-5", "main", "input", 8412300, 0, 126.18),
        cat("anthropic/claude-opus-5-5", "main", "cache_write_5m", 2104000, 0, 39.45),
        cat("anthropic/claude-opus-5-5", "main", "cache_write_1h", 310500, 0, 9.32),
        cat("anthropic/claude-opus-5-5", "main", "cache_read", 21880000, 0, 16.41),
        cat("anthropic/claude-opus-5-5", "main", "output", 1902100, 8410, 142.66),
        cat("openai/gpt-6-luna", "router", "decisions_input", 3920000, 0, 5.88),
      ],
      ledger: {},
      invoices: {},
    });
    this.months.set("2026-10", {
      month: "2026-10",
      in_progress: false,
      as_of: "2026-11-01T00:00:00Z",
      calls: 41200,
      failed_calls: 530,
      refused_calls: 21,
      tenants: [
        tenant("t-demo-a", "Demo Tenant A", 38.2, 70, { unmetered: 0.5 }),
        tenant("t-north", "North Demo Factory", 201.5, 300, { unmetered: 6 }),
      ],
      categories: [cat("anthropic/claude-opus-5-5", "main", "input", 9000000, 0, 135)],
      ledger: { Anthropic: 1204.55, OpenAI: 61.2, "Google Vertex AI": 0 },
      invoices: { Anthropic: 1206.02, OpenAI: 74.9 },
    });
  }

  failNext(method: UsageMethod, failure: ApiError): void {
    this.failures.set(method, [...(this.failures.get(method) ?? []), failure]);
  }

  api(): AiUsageApi {
    const guard = async <T>(method: UsageMethod, label: string, run: () => T): Promise<T> => {
      this.calls.push(`${method} ${label}`.trim());
      const failure = this.failures.get(method)?.shift();
      if (failure) throw failure;
      return structuredClone(run());
    };
    return {
      listMonths: () => guard("listMonths", "", () => this.listMonths()),
      getUsage: (month) => guard("getUsage", month, () => this.usage(this.month(month))),
      getReconciliation: (month) => guard("getReconciliation", month, () => this.reconciliation(this.month(month))),
      setInvoice: (month, provider, amount) =>
        guard("setInvoice", `${month} ${provider}`, () => {
          const found = this.month(month);
          if (found.in_progress) throw new ApiError(409, { code: "conflict", message_key: "error.conflict" });
          found.invoices[provider] = amount;
          return this.reconciliation(found);
        }),
    };
  }

  private month(month: string): FakeMonth {
    const found = this.months.get(month);
    if (found === undefined) throw new ApiError(404, { code: "not_found", message_key: "error.not_found" });
    return found;
  }

  private listMonths(): UsageMonth[] {
    return [...this.months.values()]
      .sort((a, b) => (a.month < b.month ? 1 : -1))
      .map(({ month, in_progress }) => ({ month, in_progress }));
  }

  private usage(month: FakeMonth): UsageSummary {
    return {
      month: month.month,
      as_of: month.as_of,
      in_progress: month.in_progress,
      totals: {
        amount_usd: round(month.tenants.reduce((sum, row) => sum + row.amount_usd, 0)),
        calls: month.calls,
        failed_calls: month.failed_calls,
        refused_calls: month.refused_calls,
        overdrawn_tenants: month.tenants.filter((row) => row.overdrawn_usd > 0).length,
        estimated_entries: month.tenants.reduce((sum, row) => sum + row.estimated_entries, 0),
        lower_bound_entries: month.tenants.reduce((sum, row) => sum + row.lower_bound_entries, 0),
      },
      tenants: month.tenants,
      categories: month.categories,
    };
  }

  private reconciliation(month: FakeMonth): Reconciliation {
    if (month.in_progress) throw new ApiError(409, { code: "conflict", message_key: "error.conflict" });
    const names = [...new Set([...Object.keys(month.ledger), ...Object.keys(month.invoices)])];
    const providers: ProviderReconciliation[] = names.map((provider) => {
      const ledger = month.ledger[provider] ?? null;
      const invoice = month.invoices[provider] ?? null;
      const status =
        invoice === null
          ? "awaiting_invoice"
          : ledger !== null && Math.abs(invoice - ledger) / Math.max(ledger, 0.01) <= TOLERANCE
            ? "within_tolerance"
            : "investigate";
      return { provider, ledger_usd: ledger === 0 ? null : ledger, invoice_usd: invoice, status };
    });
    return { month: month.month, providers };
  }
}

function round(value: number): number {
  return Math.round(value * 100) / 100;
}

function tenant(
  id: string,
  name: string,
  amount: number,
  quota: number,
  extra: {
    raise?: number;
    unmetered?: number;
    estimated?: number;
    lowerBound?: number;
    overdrawn?: number;
    blockedSince?: string;
  } = {},
): TenantUsage {
  return {
    tenant_id: id,
    name,
    amount_usd: amount,
    quota_usd: quota,
    quota_raise_usd: extra.raise ?? 0,
    unmetered_usd: extra.unmetered ?? 0,
    estimated_entries: extra.estimated ?? 0,
    lower_bound_entries: extra.lowerBound ?? 0,
    overdrawn_usd: extra.overdrawn ?? 0,
    blocked_since: extra.blockedSince ?? null,
  };
}

function cat(
  model: string,
  role: string,
  category: CategoryUsage["category"],
  tokens: number,
  estimated: number,
  amount: number,
): CategoryUsage {
  return { model, role, category, tokens, estimated_tokens: estimated, amount_usd: amount };
}

function emptyMonth(month: string, inProgress: boolean): FakeMonth {
  return {
    month,
    in_progress: inProgress,
    as_of: "2026-11-01T02:05:00Z",
    tenants: [],
    categories: [],
    calls: 0,
    failed_calls: 0,
    refused_calls: 0,
    ledger: {},
    invoices: {},
  };
}

export const preview = {
  name: "AI usage (pending #140)",
  provider: (seed: "normal" | "empty"): ComponentType<{ children: ReactNode }> => {
    const api = new FakeAiUsageServer({ seed }).api();
    return function AiUsageFake({ children }: { children: ReactNode }) {
      return createElement(AiUsageApiProvider, { api, children });
    };
  },
};
