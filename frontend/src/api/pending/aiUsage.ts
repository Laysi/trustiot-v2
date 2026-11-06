// Pending Laysi/trustiot-v2#140: replace with a module over the generated client when api/openapi.json has these endpoints.

/**
 * The platform administrator's AI usage and reconciliation API as the page needs it
 * (docs/design/platform-admin/ai-usage.html; the ledger's rules are in Laysi/trustiot-v2#140 and
 * the checklist's J8 row). Months are UTC, `YYYY-MM`.
 *
 * Assumptions, none of which the backend issue states yet: one call answers the month's summary,
 * tenants and categories; the reconciliation's status (within tolerance or to be checked) is the
 * server's, because the tolerance is its rule; the provider's invoice amount is typed in by a
 * platform administrator (decision Q11) and kept per month and provider; the CSV is built by the
 * page from the reconciliation it shows.
 */

/** A month with usage; it is in progress until its last day (UTC) is over. */
export interface UsageMonth {
  month: string;
  in_progress: boolean;
}

export type UsageCategory = "input" | "cache_write_5m" | "cache_write_1h" | "cache_read" | "output" | "decisions_input";

export interface UsageTotals {
  amount_usd: number;
  calls: number;
  failed_calls: number;
  refused_calls: number;
  overdrawn_tenants: number;
  /** Entries whose usage the gateway estimated (the provider reported none, or the stream broke). */
  estimated_entries: number;
  /** Of those, entries whose thinking length was not visible: the estimate is a lower bound. */
  lower_bound_entries: number;
}

/** One tenant's month, or the platform's own (`tenant_id` is `null`: evaluation runs and the like). */
export interface TenantUsage {
  tenant_id: string | null;
  name: string | null;
  amount_usd: number;
  /** The month's quota with a temporary raise included; `null` for the platform. */
  quota_usd: number | null;
  /** How much of the quota is a temporary raise. */
  quota_raise_usd: number;
  /** Home guidance and input completion: ledgered, not charged to the quota. `null` for the platform. */
  unmetered_usd: number | null;
  estimated_entries: number;
  lower_bound_entries: number;
  /** How far past the quota the tenant is; 0 when it is not overdrawn. */
  overdrawn_usd: number;
  /** Since when calls are refused, while overdrawn. */
  blocked_since: string | null;
}

export interface CategoryUsage {
  model: string;
  /** The model role that called it. */
  role: string;
  category: UsageCategory;
  tokens: number;
  /** Of `tokens`, the part that is estimated. */
  estimated_tokens: number;
  amount_usd: number;
}

export interface UsageSummary {
  month: string;
  /** When the figures were read; the month is in progress when it is not over yet. */
  as_of: string;
  in_progress: boolean;
  totals: UsageTotals;
  tenants: TenantUsage[];
  categories: CategoryUsage[];
}

export type ReconciliationStatus = "within_tolerance" | "investigate" | "awaiting_invoice";

export interface ProviderReconciliation {
  provider: string;
  /** What the ledger adds up to by our price table; `null` when the provider has no usage that month. */
  ledger_usd: number | null;
  invoice_usd: number | null;
  status: ReconciliationStatus;
}

export interface Reconciliation {
  month: string;
  providers: ProviderReconciliation[];
}

/**
 * The usage endpoints. Reading the ledger can fail (503): then the gateway refuses calls too, since
 * a failed quota check does not let a call through, so the page says to look at its alerts.
 */
export interface AiUsageApi {
  /** The months that have usage, newest first; the current one is always there. */
  listMonths(signal?: AbortSignal): Promise<UsageMonth[]>;
  getUsage(month: string, signal?: AbortSignal): Promise<UsageSummary>;
  /** Only for a month that is over: 409 `conflict` while it is in progress. */
  getReconciliation(month: string, signal?: AbortSignal): Promise<Reconciliation>;
  setInvoice(month: string, provider: string, amountUsd: number): Promise<Reconciliation>;
}
