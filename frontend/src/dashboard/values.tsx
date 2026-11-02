// The components that show values rather than draw marks: KPI, status and table. Every value is a
// button that opens its source.
import { focusRing } from "../components/ui/button.tsx";
import { cn } from "../lib/utils.ts";
import { Sparkline } from "./charts.tsx";
import { useDashboardEnv } from "./context.tsx";
import { type ColumnType, cellAt } from "./data.ts";
import type { KpiView, ResolvedValue, SourceRef, StatusView, TableView, Threshold } from "./model.ts";

const valueButton = cn(
  "rounded-sm text-left tabular-nums hover:underline hover:decoration-1 underline-offset-4",
  focusRing,
);

/** A value and its unit, as a button that opens the value's source. */
function ValueButton(props: { value: ResolvedValue; className?: string; onSource(source: SourceRef): void }) {
  const { t, format } = useDashboardEnv();
  return (
    <button
      type="button"
      className={cn(valueButton, props.className)}
      onClick={() => props.onSource(props.value.source)}
    >
      <span data-testid="value">{format.number(props.value.value)}</span>
      {props.value.unit !== undefined && props.value.value !== null ? (
        <span className="ml-1 text-[0.5em] font-normal text-muted-foreground">{props.value.unit}</span>
      ) : null}
      <span className="sr-only">{t("dashboard.source.open_suffix")}</span>
    </button>
  );
}

export function KpiBody(props: { view: KpiView; onSource(source: SourceRef): void }) {
  const { view, onSource } = props;
  const { format } = useDashboardEnv();
  return (
    <div className="grid gap-3">
      <div className="grid gap-1">
        <ValueButton value={view.value} onSource={onSource} className="text-4xl font-semibold tracking-tight" />
        {view.comparison !== null ? (
          <p className="flex flex-wrap items-baseline gap-x-2 text-sm text-muted-foreground">
            <span>{view.comparison.title}</span>
            <button
              type="button"
              className={cn(valueButton, "font-medium text-foreground")}
              onClick={() => onSource((view.comparison as ResolvedValue).source)}
            >
              <span data-testid="comparison">
                {format.withUnit(format.number(view.comparison.value), view.comparison.unit)}
              </span>
            </button>
          </p>
        ) : null}
      </div>
      {view.trend !== null ? (
        <Sparkline
          series={view.trend.series}
          dataset={view.trend.dataset}
          dateOnly={view.trend.dateOnly}
          unit={view.value.unit}
          title={view.title}
          onSource={onSource}
        />
      ) : null}
    </div>
  );
}

function thresholdText(
  threshold: Threshold,
  unit: string | undefined,
  format: ReturnType<typeof useDashboardEnv>["format"],
) {
  return `${threshold.title} ${format.withUnit(format.number(threshold.value), threshold.unit ?? unit)}`;
}

/**
 * A value against thresholds: which band it is in, in words, and the bands as equal segments with
 * the value's band marked. The segments do not place the value inside its band (that would be a
 * computed position), and no band is colored as good or bad: the spec does not say which is which.
 */
export function StatusBody(props: { view: StatusView; onSource(source: SourceRef): void }) {
  const { view, onSource } = props;
  const { t, format } = useDashboardEnv();
  const unit = view.value.unit;
  const bands = view.thresholds.length + 1;
  let bandText: string;
  if (view.band === null) {
    bandText = t("dashboard.status.no_value");
  } else if (view.band === 0) {
    bandText = t("dashboard.status.below", { threshold: thresholdText(view.thresholds[0] as Threshold, unit, format) });
  } else if (view.band === view.thresholds.length) {
    bandText = t("dashboard.status.at_or_above", {
      threshold: thresholdText(view.thresholds[view.band - 1] as Threshold, unit, format),
    });
  } else {
    bandText = t("dashboard.status.between", {
      low: thresholdText(view.thresholds[view.band - 1] as Threshold, unit, format),
      high: thresholdText(view.thresholds[view.band] as Threshold, unit, format),
    });
  }
  return (
    <div className="grid gap-3">
      <div className="grid gap-1">
        <ValueButton value={view.value} onSource={onSource} className="text-4xl font-semibold tracking-tight" />
        <p className="text-sm font-medium" data-testid="status-band">
          {bandText}
        </p>
      </div>
      <div aria-hidden="true" className="flex gap-0.5">
        {Array.from({ length: bands }, (_, band) => (
          <span
            // biome-ignore lint/suspicious/noArrayIndexKey: the bands are positions, fixed by the threshold count
            key={band}
            className={cn(
              "h-2 flex-1 first:rounded-l-full last:rounded-r-full",
              band === view.band ? "bg-primary" : "bg-muted",
            )}
          />
        ))}
      </div>
      <ul className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
        {view.thresholds.map((threshold) => (
          <li key={threshold.name}>
            <button
              type="button"
              className={cn(valueButton)}
              onClick={() => onSource({ kind: "constant", name: threshold.name })}
            >
              <span data-testid="threshold">{thresholdText(threshold, unit, format)}</span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** Text longer than this wraps inside a bounded width; anything shorter stays on one line. */
const MAX_UNWRAPPED_TEXT = 24;
const WRAPPING_CELL = "min-w-48 max-w-80 break-words";

/**
 * Numbers, times and yes/no never wrap, nor does short text, so a narrow screen scrolls the table
 * sideways instead of breaking "Nov 1, 2026, 1:02:03 AM" over four lines.
 */
function wraps(type: ColumnType, text: string): boolean {
  return type === "text" && text.length > MAX_UNWRAPPED_TEXT;
}

/** Every row of the result, each cell a button that opens its row's source. */
export function TableBody(props: { view: TableView; onSource(source: SourceRef): void }) {
  const { view, onSource } = props;
  const { t, format } = useDashboardEnv();
  const rows = view.result.rows;
  return (
    <div className="max-h-96 overflow-auto rounded-md border">
      <table className="w-full border-collapse text-sm">
        <caption className="sr-only">{view.title}</caption>
        <thead className="sticky top-0 bg-card">
          <tr>
            {view.columns.map((column) => (
              <th
                key={column.name}
                scope="col"
                className={cn(
                  "border-b px-3 py-2 text-xs font-medium whitespace-nowrap text-muted-foreground",
                  column.type === "number" ? "text-right" : "text-left",
                )}
              >
                {column.name}
                {column.unit !== undefined ? <span className="ml-1 font-normal">({column.unit})</span> : null}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.length === 0 ? (
            <tr>
              <td colSpan={view.columns.length} className="px-3 py-6 text-center text-muted-foreground">
                {t("dashboard.empty")}
              </td>
            </tr>
          ) : (
            rows.map((_, row) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: a row's position is its identity in a result
              <tr key={row} className="border-b last:border-b-0 hover:bg-muted/50">
                {view.columns.map((column) => {
                  const text = format.cell(column.type, cellAt(view.result, row, column.index));
                  return (
                    <td
                      key={column.name}
                      className={cn("px-3 py-1.5 align-top", column.type === "number" ? "text-right" : "text-left")}
                    >
                      <button
                        type="button"
                        className={cn(valueButton, wraps(column.type, text) ? WRAPPING_CELL : "whitespace-nowrap")}
                        onClick={() => onSource({ kind: "rows", dataset: view.dataset, rows: [row] })}
                        data-testid="table-cell"
                      >
                        {text}
                      </button>
                    </td>
                  );
                })}
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}
