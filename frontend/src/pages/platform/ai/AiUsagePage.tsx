import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CircleAlertIcon, DownloadIcon, WalletIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { Link } from "react-router";

import { useAiUsageApi } from "../../../api/pending/AiUsageApiProvider.tsx";
import type { ProviderReconciliation, ReconciliationStatus, UsageSummary } from "../../../api/pending/aiUsage.ts";
import { EmptyState } from "../../../components/EmptyState.tsx";
import { Alert, AlertDescription, AlertTitle } from "../../../components/ui/alert.tsx";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "../../../components/ui/alert-dialog.tsx";
import { Button, buttonVariants } from "../../../components/ui/button.tsx";
import { Card, CardContent, CardHeader, CardTitle } from "../../../components/ui/card.tsx";
import { FormField } from "../../../components/ui/form-field.tsx";
import { Input } from "../../../components/ui/input.tsx";
import { NativeSelect } from "../../../components/ui/native-select.tsx";
import { Spinner } from "../../../components/ui/spinner.tsx";
import type { MessageId } from "../../../i18n/messages.ts";
import { useI18n } from "../../../i18n/useI18n.ts";
import { cn } from "../../../lib/utils.ts";
import { PageHeader } from "../../dashboards/PageHeader.tsx";
import { reconciliationKey, usageKey, usageMonthsKey } from "./queries.ts";
import { difference, reconciliationCsv } from "./reconciliation.ts";
import { Badge, Cell, DataTable, Loading, Meter, Row, Skeleton, StatTile, Tabs, Th } from "./ui.tsx";

const CATEGORY_TEXT: Record<UsageSummary["categories"][number]["category"], MessageId> = {
  input: "platform_ai.usage.cat.input",
  cache_write_5m: "platform_ai.usage.cat.cache_write_5m",
  cache_write_1h: "platform_ai.usage.cat.cache_write_1h",
  cache_read: "platform_ai.usage.cat.cache_read",
  output: "platform_ai.usage.cat.output",
  decisions_input: "platform_ai.usage.cat.decisions_input",
};

const STATUS: Record<ReconciliationStatus, { text: MessageId; tone: "success" | "danger" | "neutral" }> = {
  within_tolerance: { text: "platform_ai.usage.status.within_tolerance", tone: "success" },
  investigate: { text: "platform_ai.usage.status.investigate", tone: "danger" },
  awaiting_invoice: { text: "platform_ai.usage.status.awaiting_invoice", tone: "neutral" },
};

/** Where the platform administration's tenant page is, for the quota raise. */
function tenantPath(tenantId: string): string {
  return `/platform/tenants/${encodeURIComponent(tenantId)}`;
}

function useMoney() {
  const { locale } = useI18n();
  return (usd: number) =>
    `US$${new Intl.NumberFormat(locale, { minimumFractionDigits: 2, maximumFractionDigits: 2 }).format(usd)}`;
}

function useMonthLabel() {
  const { locale } = useI18n();
  return (month: string) =>
    new Intl.DateTimeFormat(locale, { year: "numeric", month: "long", timeZone: "UTC" }).format(
      new Date(`${month}-01T00:00:00Z`),
    );
}

function TenantsTab({ usage }: { usage: UsageSummary }) {
  const { t, formatDateTime, formatNumber } = useI18n();
  const money = useMoney();
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<"utilization" | "amount">("utilization");

  const rows = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const share = (row: UsageSummary["tenants"][number]) =>
      row.quota_usd === null || row.quota_usd === 0 ? 0 : row.amount_usd / row.quota_usd;
    const tenants = usage.tenants
      .filter((row) => row.tenant_id !== null)
      .filter(
        (row) =>
          needle === "" ||
          (row.name ?? "").toLowerCase().includes(needle) ||
          (row.tenant_id ?? "").toLowerCase().includes(needle),
      )
      .sort((a, b) => (sort === "utilization" ? share(b) - share(a) : b.amount_usd - a.amount_usd));
    const platform = needle === "" ? usage.tenants.filter((row) => row.tenant_id === null) : [];
    return [...tenants, ...platform];
  }, [usage.tenants, query, sort]);

  if (usage.tenants.length === 0) {
    return (
      <EmptyState centered icon={<WalletIcon />} title={t("platform_ai.usage.empty_title")}>
        <p>{t("platform_ai.usage.empty_body")}</p>
      </EmptyState>
    );
  }

  return (
    <div className="grid gap-3">
      <search aria-label={t("platform_ai.usage.filter_label")} className="flex flex-wrap items-end gap-3">
        <FormField label={t("platform_ai.usage.filter_label")} className="min-w-48 flex-1">
          {(control) => (
            <Input
              {...control}
              type="search"
              value={query}
              placeholder={t("platform_ai.usage.filter_placeholder")}
              onChange={(event) => setQuery(event.target.value)}
            />
          )}
        </FormField>
        <FormField label={t("platform_ai.usage.sort_label")} className="w-64 max-w-full">
          {(control) => (
            <NativeSelect
              {...control}
              value={sort}
              onChange={(event) => setSort(event.target.value === "amount" ? "amount" : "utilization")}
            >
              <option value="utilization">{t("platform_ai.usage.sort_utilization")}</option>
              <option value="amount">{t("platform_ai.usage.sort_amount")}</option>
            </NativeSelect>
          )}
        </FormField>
      </search>
      {rows.length === 0 ? (
        <p className="text-sm text-muted-foreground">{t("platform_ai.usage.no_match")}</p>
      ) : (
        <DataTable
          caption={t("platform_ai.usage.caption_tenants")}
          head={
            <>
              <Th>{t("platform_ai.usage.col_tenant")}</Th>
              <Th>{t("platform_ai.usage.col_quota")}</Th>
              <Th>{t("platform_ai.usage.col_util")}</Th>
              <Th>{t("platform_ai.usage.col_unmetered")}</Th>
              <Th>{t("platform_ai.usage.col_estimated")}</Th>
              <Th>
                <span className="sr-only">{t("platform_ai.usage.col_actions")}</span>
              </Th>
            </>
          }
        >
          {rows.map((row) => {
            const percent = row.quota_usd ? Math.round((row.amount_usd / row.quota_usd) * 100) : null;
            const overdrawn = row.overdrawn_usd > 0;
            return (
              <Row key={row.tenant_id ?? "platform"}>
                <Cell label={t("platform_ai.usage.col_tenant")}>
                  {row.tenant_id === null ? (
                    <span className="grid">
                      <strong>{t("platform_ai.usage.platform")}</strong>
                      <span className="text-xs text-muted-foreground">{t("platform_ai.usage.platform_note")}</span>
                    </span>
                  ) : (
                    <span className="grid gap-0.5">
                      <span className="flex flex-wrap items-center gap-1.5">
                        <strong className="font-medium">{row.name ?? row.tenant_id}</strong>
                        {overdrawn ? (
                          <Badge tone="danger" icon={<CircleAlertIcon aria-hidden="true" />}>
                            {t("platform_ai.usage.overdrawn")}
                          </Badge>
                        ) : null}
                      </span>
                      {overdrawn ? (
                        <span className="text-xs text-muted-foreground">
                          {t("platform_ai.usage.overdrawn_note", {
                            amount: money(row.overdrawn_usd),
                            time: row.blocked_since ? formatDateTime(row.blocked_since) : "",
                          })}
                        </span>
                      ) : null}
                    </span>
                  )}
                </Cell>
                <Cell label={t("platform_ai.usage.col_quota")}>
                  <span className="grid tabular-nums">
                    {row.quota_usd === null
                      ? `${money(row.amount_usd)} / —`
                      : `${money(row.amount_usd)} / ${money(row.quota_usd)}`}
                    {row.quota_raise_usd > 0 ? (
                      <span className="text-xs text-muted-foreground">
                        {t("platform_ai.usage.raised", { amount: money(row.quota_raise_usd) })}
                      </span>
                    ) : null}
                  </span>
                </Cell>
                <Cell label={t("platform_ai.usage.col_util")}>
                  {percent === null ? (
                    "—"
                  ) : (
                    <span className="grid min-w-24">
                      <span className="tabular-nums">{percent}%</span>
                      <Meter percent={percent} tone={percent > 100 ? "danger" : percent >= 80 ? "warning" : "normal"} />
                    </span>
                  )}
                </Cell>
                <Cell label={t("platform_ai.usage.col_unmetered")} className="tabular-nums">
                  {row.unmetered_usd === null ? "—" : money(row.unmetered_usd)}
                </Cell>
                <Cell label={t("platform_ai.usage.col_estimated")}>
                  <span className="inline-flex flex-wrap items-center gap-1.5 tabular-nums">
                    {row.estimated_entries === 0
                      ? "0"
                      : t("platform_ai.usage.estimated_count", { count: formatNumber(row.estimated_entries) })}
                    {row.lower_bound_entries > 0 ? (
                      <Badge tone="outline">
                        {t("platform_ai.usage.lower_bound", { count: row.lower_bound_entries })}
                      </Badge>
                    ) : null}
                  </span>
                </Cell>
                <Cell label={t("platform_ai.usage.col_actions")}>
                  {overdrawn && row.tenant_id !== null ? (
                    <Link
                      to={tenantPath(row.tenant_id)}
                      aria-label={t("platform_ai.usage.raise_quota_label", { tenant: row.name ?? row.tenant_id })}
                      className={cn(buttonVariants({ variant: "outline", size: "sm" }))}
                    >
                      {t("platform_ai.usage.raise_quota")}
                    </Link>
                  ) : null}
                </Cell>
              </Row>
            );
          })}
        </DataTable>
      )}
      <p className="text-xs text-muted-foreground">{t("platform_ai.usage.tenants_note")}</p>
    </div>
  );
}

function CategoriesTab({ usage, month }: { usage: UsageSummary; month: string }) {
  const { t, formatNumber } = useI18n();
  const money = useMoney();
  if (usage.categories.length === 0) {
    return (
      <EmptyState centered icon={<WalletIcon />} title={t("platform_ai.usage.empty_title")}>
        <p>{t("platform_ai.usage.empty_body")}</p>
      </EmptyState>
    );
  }
  return (
    <DataTable
      caption={t("platform_ai.usage.caption_categories", { month })}
      head={
        <>
          <Th>{t("platform_ai.usage.col_model")}</Th>
          <Th>{t("platform_ai.usage.col_category")}</Th>
          <Th align="right">{t("platform_ai.usage.col_tokens")}</Th>
          <Th align="right">{t("platform_ai.usage.col_amount")}</Th>
        </>
      }
    >
      {usage.categories.map((row, index) => {
        const previous = usage.categories[index - 1];
        const sameModel = previous?.model === row.model && previous.role === row.role;
        return (
          <Row key={`${row.model}/${row.role}/${row.category}`}>
            <Cell label={t("platform_ai.usage.col_model")}>
              {sameModel ? null : (
                <span className="grid">
                  <span className="font-mono">{row.model}</span>
                  <span className="text-xs text-muted-foreground">{row.role}</span>
                </span>
              )}
            </Cell>
            <Cell label={t("platform_ai.usage.col_category")}>{t(CATEGORY_TEXT[row.category])}</Cell>
            <Cell label={t("platform_ai.usage.col_tokens")} align="right">
              <span className="inline-flex flex-wrap items-center justify-end gap-1.5">
                {formatNumber(row.tokens)}
                {row.estimated_tokens > 0 ? (
                  <Badge tone="outline">
                    {t("platform_ai.usage.includes_estimated", { count: formatNumber(row.estimated_tokens) })}
                  </Badge>
                ) : null}
              </span>
            </Cell>
            <Cell label={t("platform_ai.usage.col_amount")} align="right">
              {money(row.amount_usd)}
            </Cell>
          </Row>
        );
      })}
    </DataTable>
  );
}

function InvoiceDialog({
  month,
  monthLabel,
  row,
  onClose,
}: {
  month: string;
  monthLabel: string;
  row: ProviderReconciliation;
  onClose: () => void;
}) {
  const { t, errorMessage } = useI18n();
  const api = useAiUsageApi();
  const queryClient = useQueryClient();
  const [amount, setAmount] = useState(row.invoice_usd === null ? "" : String(row.invoice_usd));
  const [touched, setTouched] = useState(false);
  const parsed = Number(amount.trim());
  const valid = amount.trim() !== "" && Number.isFinite(parsed) && parsed > 0;
  const save = useMutation({
    mutationFn: () => api.setInvoice(month, row.provider, parsed),
    onSuccess: (updated) => {
      queryClient.setQueryData(reconciliationKey(month), updated);
      onClose();
    },
  });
  return (
    <AlertDialog
      open
      onOpenChange={(open) => {
        if (!open && !save.isPending) onClose();
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>
            {t("platform_ai.usage.invoice_title", { provider: row.provider, month: monthLabel })}
          </AlertDialogTitle>
          <AlertDialogDescription>{t("platform_ai.usage.invoice_desc")}</AlertDialogDescription>
        </AlertDialogHeader>
        <FormField
          label={t("platform_ai.usage.invoice_label")}
          error={touched && !valid ? t("platform_ai.usage.invoice_invalid") : undefined}
        >
          {(control) => (
            <Input
              {...control}
              inputMode="decimal"
              value={amount}
              onChange={(event) => setAmount(event.target.value)}
              onBlur={() => setTouched(true)}
            />
          )}
        </FormField>
        {save.isError ? (
          <Alert variant="destructive">
            <AlertDescription>
              <p>{errorMessage(save.error)}</p>
            </AlertDescription>
          </Alert>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={save.isPending}>{t("platform_ai.cancel")}</AlertDialogCancel>
          <Button
            disabled={save.isPending}
            onClick={() => {
              setTouched(true);
              if (valid) save.mutate();
            }}
          >
            {save.isPending ? <Spinner /> : null}
            {save.isPending ? t("platform_ai.usage.invoice_pending") : t("platform_ai.usage.invoice_submit")}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/** Hands the browser a CSV to save; where the browser cannot (no object URLs), it does nothing. */
function saveCsv(name: string, text: string): void {
  if (typeof URL.createObjectURL !== "function") return;
  const url = URL.createObjectURL(new Blob([text], { type: "text/csv" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.click();
  URL.revokeObjectURL(url);
}

function ReconciliationTab({ usage, month }: { usage: UsageSummary; month: string }) {
  const { t, errorMessage } = useI18n();
  const money = useMoney();
  const monthLabel = useMonthLabel()(month);
  const api = useAiUsageApi();
  const [editing, setEditing] = useState<ProviderReconciliation | null>(null);
  const recon = useQuery({
    queryKey: reconciliationKey(month),
    queryFn: () => api.getReconciliation(month),
    enabled: !usage.in_progress,
  });

  if (usage.in_progress) {
    return (
      <Alert role="status">
        <AlertTitle>{t("platform_ai.usage.in_progress_title", { month: monthLabel })}</AlertTitle>
        <AlertDescription>
          <p>{t("platform_ai.usage.in_progress_body")}</p>
        </AlertDescription>
      </Alert>
    );
  }
  if (recon.isPending) return <Skeleton className="h-40" />;
  if (recon.isError) {
    return (
      <Alert variant="destructive">
        <AlertTitle>{t("platform_ai.usage.load_failed")}</AlertTitle>
        <AlertDescription>
          <p>{errorMessage(recon.error)}</p>
          <Button size="sm" variant="outline" onClick={() => void recon.refetch()}>
            {t("platform_ai.roles.retry")}
          </Button>
        </AlertDescription>
      </Alert>
    );
  }
  const rows = recon.data.providers;
  return (
    <Card className="gap-4 py-5">
      <CardHeader>
        <div className="grid gap-0.5">
          <CardTitle>{t("platform_ai.usage.recon_title", { month: monthLabel })}</CardTitle>
          <p className="text-sm text-muted-foreground">{t("platform_ai.usage.recon_desc")}</p>
        </div>
        <Button
          variant="outline"
          size="sm"
          disabled={rows.length === 0}
          onClick={() => saveCsv(`ai-reconciliation-${month}.csv`, reconciliationCsv(month, rows))}
        >
          <DownloadIcon aria-hidden="true" />
          {t("platform_ai.usage.download")}
        </Button>
      </CardHeader>
      <CardContent>
        {rows.length === 0 ? (
          <p className="text-sm text-muted-foreground">{t("platform_ai.usage.recon_empty")}</p>
        ) : (
          <DataTable
            caption={t("platform_ai.usage.recon_title", { month: monthLabel })}
            head={
              <>
                <Th>{t("platform_ai.usage.col_provider")}</Th>
                <Th align="right">{t("platform_ai.usage.col_ledger")}</Th>
                <Th align="right">{t("platform_ai.usage.col_invoice")}</Th>
                <Th align="right">{t("platform_ai.usage.col_diff")}</Th>
                <Th>{t("platform_ai.usage.col_status")}</Th>
              </>
            }
          >
            {rows.map((row) => {
              const diff = difference(row);
              const status = STATUS[row.status];
              return (
                <Row key={row.provider}>
                  <Cell label={t("platform_ai.usage.col_provider")}>
                    <strong className="font-medium">{row.provider}</strong>
                  </Cell>
                  <Cell label={t("platform_ai.usage.col_ledger")} align="right">
                    {row.ledger_usd === null ? "—" : money(row.ledger_usd)}
                  </Cell>
                  <Cell label={t("platform_ai.usage.col_invoice")} align="right">
                    {row.invoice_usd === null ? (
                      <Button variant="outline" size="sm" onClick={() => setEditing(row)}>
                        {t("platform_ai.usage.enter_invoice")}
                      </Button>
                    ) : (
                      <span className="inline-flex items-center gap-2">
                        {money(row.invoice_usd)}
                        <Button variant="ghost" size="sm" onClick={() => setEditing(row)}>
                          {t("platform_ai.usage.edit_invoice")}
                        </Button>
                      </span>
                    )}
                  </Cell>
                  <Cell label={t("platform_ai.usage.col_diff")} align="right">
                    {diff === null
                      ? "—"
                      : `${diff.usd >= 0 ? "+" : "−"}${money(Math.abs(diff.usd))}（${Math.abs(diff.percent).toFixed(2)}%）`}
                  </Cell>
                  <Cell label={t("platform_ai.usage.col_status")}>
                    <Badge tone={status.tone}>{t(status.text)}</Badge>
                  </Cell>
                </Row>
              );
            })}
          </DataTable>
        )}
      </CardContent>
      {editing ? (
        <InvoiceDialog month={month} monthLabel={monthLabel} row={editing} onClose={() => setEditing(null)} />
      ) : null}
    </Card>
  );
}

/**
 * AI usage and reconciliation (`/platform/ai/usage`, checklist J8; design:
 * docs/design/platform-admin/ai-usage.html): the month's figures from the AI gateway's ledger,
 * by tenant and by category, and for a month that is over, the ledger beside each provider's
 * invoice, which the administrator types in (decision Q11).
 */
export function AiUsagePage() {
  const { t, errorMessage, formatDateTime, formatNumber } = useI18n();
  const money = useMoney();
  const monthLabel = useMonthLabel();
  const api = useAiUsageApi();
  const [chosen, setChosen] = useState<string | null>(null);
  const [tab, setTab] = useState("tenants");

  const months = useQuery({ queryKey: usageMonthsKey, queryFn: ({ signal }) => api.listMonths(signal) });
  const month = chosen ?? months.data?.[0]?.month ?? null;
  const usage = useQuery({
    queryKey: usageKey(month ?? ""),
    queryFn: ({ signal }) => api.getUsage(month ?? "", signal),
    enabled: month !== null,
  });

  const header = (
    <PageHeader
      title={t("platform_ai.usage.title")}
      meta={t("platform_ai.usage.intro")}
      actions={
        months.data && months.data.length > 0 && month !== null ? (
          <FormField label={t("platform_ai.usage.month_label")} className="min-w-52">
            {(control) => (
              <NativeSelect {...control} value={month} onChange={(event) => setChosen(event.target.value)}>
                {months.data.map((item) => (
                  <option key={item.month} value={item.month}>
                    {item.in_progress
                      ? t("platform_ai.usage.month_in_progress", { month: monthLabel(item.month) })
                      : monthLabel(item.month)}
                  </option>
                ))}
              </NativeSelect>
            )}
          </FormField>
        ) : null
      }
    />
  );

  if (months.isPending || (month !== null && usage.isPending)) {
    return (
      <div className="grid gap-6">
        {header}
        <Loading label={t("app.loading")} className="grid gap-3">
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <Skeleton className="h-24" />
            <Skeleton className="h-24" />
            <Skeleton className="h-24" />
            <Skeleton className="h-24" />
          </div>
          <Skeleton className="h-48" />
        </Loading>
      </div>
    );
  }
  if (months.isError || usage.isError || usage.data === undefined) {
    return (
      <div className="grid gap-6">
        {header}
        <Alert variant="destructive">
          <AlertTitle>{t("platform_ai.usage.load_failed")}</AlertTitle>
          <AlertDescription>
            <p>{errorMessage(months.error ?? usage.error)}</p>
            <p>{t("platform_ai.usage.load_failed_body")}</p>
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                void months.refetch();
                void usage.refetch();
              }}
            >
              {t("platform_ai.roles.retry")}
            </Button>
          </AlertDescription>
        </Alert>
      </div>
    );
  }
  const data = usage.data;
  const label = monthLabel(data.month);
  const { totals } = data;

  return (
    <div className="grid gap-6">
      {header}
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <StatTile
          label={t("platform_ai.usage.tile_amount")}
          value={money(totals.amount_usd)}
          note={t("platform_ai.usage.tile_amount_note", { time: formatDateTime(data.as_of) })}
        />
        <StatTile
          label={t("platform_ai.usage.tile_calls")}
          value={formatNumber(totals.calls)}
          note={t("platform_ai.usage.tile_calls_note", {
            failed: formatNumber(totals.failed_calls),
            refused: formatNumber(totals.refused_calls),
          })}
        />
        <StatTile
          label={t("platform_ai.usage.tile_overdrawn")}
          value={formatNumber(totals.overdrawn_tenants)}
          note={t("platform_ai.usage.tile_overdrawn_note")}
        />
        <StatTile
          label={t("platform_ai.usage.tile_estimated")}
          value={formatNumber(totals.estimated_entries)}
          note={t("platform_ai.usage.tile_estimated_note", { count: formatNumber(totals.lower_bound_entries) })}
        />
      </div>
      <Tabs
        label={t("platform_ai.usage.tabs_label")}
        value={tab}
        onChange={setTab}
        tabs={[
          { id: "tenants", label: t("platform_ai.usage.tab_tenants") },
          { id: "categories", label: t("platform_ai.usage.tab_categories") },
          { id: "reconciliation", label: t("platform_ai.usage.tab_recon") },
        ]}
      >
        {tab === "tenants" ? <TenantsTab usage={data} /> : null}
        {tab === "categories" ? <CategoriesTab usage={data} month={label} /> : null}
        {tab === "reconciliation" ? <ReconciliationTab usage={data} month={data.month} /> : null}
      </Tabs>
    </div>
  );
}
